//! One `arcbox-hv-vmm` process, as the driver sees it: its pid, its control
//! socket, its exit, and the events it reports.
//!
//! A spawned process is reaped by a waiter task the moment it exits, so a
//! pid is never signalled after it could have been reused; its exit status
//! is the wait status. An adopted process is not this process's child: its
//! exit is the end of its event stream (its sockets close when it dies),
//! and its status is what it reported on that stream, or `SIGKILL` when
//! the driver killed it.
//!
//! The event stream (`Request::Watch`) is read by one thread per process,
//! for the whole life of the process, prepared or booted.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcbox_vm_driver::{ExitStatus, Result, VmEvent};
use tokio::sync::{broadcast, watch};

use crate::proto::{Event, Request};
use crate::{driver_error, wire};

/// How long a kill waits for the process to be gone.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// The status of a process whose real status was never observed.
const UNKNOWN_EXIT: ExitStatus = ExitStatus {
    code: None,
    signal: None,
};

/// A VMM process shared by the prepared VM and the handle that runs on it.
pub struct VmmProcess {
    pid: u32,
    socket: PathBuf,
    adopted: bool,
    exit: watch::Sender<Option<ExitStatus>>,
    events: broadcast::Sender<VmEvent>,
    /// Set before this side kills the process.
    killed: AtomicBool,
    /// Set by a detach: nothing here kills the process any more.
    detached: AtomicBool,
    /// What the process said its exit status is, before it went.
    reported: Mutex<Option<ExitStatus>>,
}

impl VmmProcess {
    /// Takes a spawned child: a task reaps it and publishes its exit.
    pub fn spawned(mut child: tokio::process::Child, socket: PathBuf) -> Result<Arc<Self>> {
        let pid = child
            .id()
            .ok_or_else(|| driver_error("spawn", std::io::Error::other("no pid")))?;
        let process = Arc::new(Self::new(pid, socket, false));
        let exit = process.exit.clone();
        let events = process.events.clone();
        tokio::spawn(async move {
            let status = match child.wait().await {
                Ok(status) => ExitStatus::from(status),
                Err(e) => {
                    tracing::warn!(pid, "waiting for arcbox-hv-vmm failed: {e}");
                    UNKNOWN_EXIT
                }
            };
            publish(&exit, &events, status);
        });
        Ok(process)
    }

    /// Tracks a process found running; its exit is the end of its events.
    pub fn adopted(pid: u32, socket: PathBuf) -> Arc<Self> {
        Arc::new(Self::new(pid, socket, true))
    }

    fn new(pid: u32, socket: PathBuf, adopted: bool) -> Self {
        Self {
            pid,
            socket,
            adopted,
            exit: watch::channel(None).0,
            events: broadcast::channel(16).0,
            killed: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            reported: Mutex::new(None),
        }
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Starts reading the process's event stream.
    pub fn watch(self: &Arc<Self>) -> Result<()> {
        let conn = UnixStream::connect(&self.socket)
            .map_err(|e| driver_error("watch the VMM process", e))?;
        wire::send(&conn, &Request::Watch, None).map_err(|e| driver_error("watch", e))?;
        let process = Arc::clone(self);
        std::thread::Builder::new()
            .name("hv-driver-watch".into())
            .spawn(move || process.read_events(&conn))
            .map_err(|e| driver_error("watch thread", e))?;
        Ok(())
    }

    fn read_events(&self, conn: &UnixStream) {
        while let Ok((event, _)) = wire::recv::<Event>(conn) {
            match event {
                Event::ResetRequested => {
                    let _ = self.events.send(VmEvent::ResetRequested);
                }
                Event::Exited(status) => *lock(&self.reported) = Some(status),
            }
        }
        // The stream ends when the process does.
        if self.adopted {
            let status = if self.killed.load(Ordering::SeqCst) {
                ExitStatus::signaled(libc::SIGKILL)
            } else {
                lock(&self.reported).unwrap_or(UNKNOWN_EXIT)
            };
            publish(&self.exit, &self.events, status);
        }
    }

    /// How the process ended, once it has.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        *self.exit.borrow()
    }

    pub fn alive(&self) -> bool {
        self.exit_status().is_none()
    }

    pub fn events(&self) -> broadcast::Receiver<VmEvent> {
        self.events.subscribe()
    }

    /// Waits up to `timeout` for the exit.
    pub async fn wait(&self, timeout: Duration) -> Option<ExitStatus> {
        let mut exit = self.exit.subscribe();
        tokio::time::timeout(timeout, exit.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(|status| status.ok().and_then(|s| *s))
    }

    /// Kills the process unless it already exited, and waits until it is
    /// gone. Idempotent.
    pub async fn kill(&self) -> Result<ExitStatus> {
        self.signal_kill();
        self.wait(REAP_TIMEOUT).await.ok_or_else(|| {
            driver_error(
                format!("arcbox-hv-vmm {} survived SIGKILL", self.pid),
                std::io::Error::from(std::io::ErrorKind::TimedOut),
            )
        })
    }

    /// Sends `SIGKILL` unless the process exited or was detached.
    pub fn signal_kill(&self) {
        if !self.alive() || self.detached.load(Ordering::SeqCst) {
            return;
        }
        self.killed.store(true, Ordering::SeqCst);
        let pid = libc::pid_t::try_from(self.pid).unwrap_or(libc::pid_t::MAX);
        // SAFETY: plain syscall. A spawned pid is not reaped (so not
        // reusable) before its exit is published, which `alive` checked;
        // an adopted one is held by its open event stream.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            tracing::debug!(pid, "SIGKILL: {}", std::io::Error::last_os_error());
        }
    }

    /// Gives the process up: nothing here kills it any more.
    pub fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
    }
}

/// Records the exit once and announces it.
fn publish(
    exit: &watch::Sender<Option<ExitStatus>>,
    events: &broadcast::Sender<VmEvent>,
    status: ExitStatus,
) {
    let first = exit.send_if_modified(|slot| {
        if slot.is_some() {
            return false;
        }
        *slot = Some(status);
        true
    });
    if first {
        let _ = events.send(VmEvent::Exited(status));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
