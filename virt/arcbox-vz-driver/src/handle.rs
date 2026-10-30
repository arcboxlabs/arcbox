//! The port's `VmHandle` over a running VZ VM.
//!
//! VZ reports no stop event, so a watcher polls the VM's state and turns
//! the first stop it sees into the one `VmEvent::Exited`. The host side of
//! the network is dropped only after the VM has stopped: the datapath and
//! relays stop, and the descriptors VZ borrowed close.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use arcbox_vm_driver::{
    Balloon, BalloonStats, Console, DebugSnapshot, Error, ExitStatus, IoMode, PortForward, Result,
    ShutdownMode, VmEvent, VmHandle, VmId, VmRecord, VmState, Vsock, VsockConn,
};
use arcbox_vz::{VirtualMachine, VirtualMachineState};
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::console::PipeConsole;
use crate::net::NetResources;
use crate::port_forward::InboundForward;
use crate::{NAME, driver_error};

/// How often the watcher looks for a stop nobody asked for.
const WATCH_INTERVAL: Duration = Duration::from_millis(250);
/// How long a vsock dial waits for the guest to accept.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A running VZ VM. Dropping it stops the VM unless it already exited.
pub struct VzHandle {
    inner: Arc<Inner>,
    watcher: tokio::task::JoinHandle<()>,
}

struct Inner {
    record: VmRecord,
    vm: VirtualMachine,
    exit: Mutex<Option<ExitStatus>>,
    events: broadcast::Sender<VmEvent>,
    console: Option<PipeConsole>,
    agent_log: Option<PipeConsole>,
    vsock: bool,
    balloon: bool,
    /// The NICs' host side, until the VM exits.
    net: Mutex<Option<NetResources>>,
}

/// What a running VM keeps besides the VM itself.
pub struct Parts {
    pub record: VmRecord,
    pub console: Option<PipeConsole>,
    pub agent_log: Option<PipeConsole>,
    pub net: NetResources,
    pub vsock: bool,
    pub balloon: bool,
}

impl VzHandle {
    /// Takes ownership of a started `vm`.
    pub fn new(vm: VirtualMachine, parts: Parts) -> Self {
        let (events, _) = broadcast::channel(16);
        let inner = Arc::new(Inner {
            record: parts.record,
            vm,
            exit: Mutex::new(None),
            events,
            console: parts.console,
            agent_log: parts.agent_log,
            vsock: parts.vsock,
            balloon: parts.balloon,
            net: Mutex::new(Some(parts.net)),
        });
        let watcher = tokio::spawn(watch(Arc::downgrade(&inner)));
        Self { inner, watcher }
    }
}

/// Records the first stop the VM reaches on its own.
async fn watch(inner: Weak<Inner>) {
    let mut tick = tokio::time::interval(WATCH_INTERVAL);
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        if inner.exited().is_some() {
            return;
        }
        if let Some(status) = stopped_status(inner.vm.state()) {
            inner.exit(status);
            return;
        }
    }
}

/// The status a VM that stopped on its own exited with; `None` while it
/// has not stopped.
fn stopped_status(state: VirtualMachineState) -> Option<ExitStatus> {
    match state {
        VirtualMachineState::Stopped => Some(ExitStatus::exited(0)),
        VirtualMachineState::Error => Some(ExitStatus::exited(1)),
        _ => None,
    }
}

impl Inner {
    fn exited(&self) -> Option<ExitStatus> {
        *lock(&self.exit)
    }

    /// Records the exit once and releases the network; later calls return
    /// the first status.
    fn exit(&self, status: ExitStatus) -> ExitStatus {
        {
            let mut exit = lock(&self.exit);
            if let Some(earlier) = *exit {
                return earlier;
            }
            *exit = Some(status);
        }
        drop(lock(&self.net).take());
        // A send error only means nobody is subscribed.
        let _ = self.events.send(VmEvent::Exited(status));
        tracing::info!(vm = %self.record.id, %status, "VZ VM exited");
        status
    }

    fn require_running(&self) -> Result<()> {
        match self.exited() {
            None => Ok(()),
            Some(status) => Err(Error::WrongState {
                id: self.record.id.clone(),
                state: VmState::Exited(status),
                expected: "running",
            }),
        }
    }

    /// Stops the VM without asking the guest.
    async fn kill(&self) -> Result<ExitStatus> {
        if let Some(status) = self.exited() {
            return Ok(status);
        }
        if let Some(status) = stopped_status(self.vm.state()) {
            return Ok(self.exit(status));
        }
        if let Err(error) = self.vm.stop().await {
            // A guest that halted while the stop was issued is stopped too.
            if let Some(status) = stopped_status(self.vm.state()) {
                return Ok(self.exit(status));
            }
            return Err(driver_error("stop", error));
        }
        Ok(self.exit(ExitStatus::signaled(libc::SIGKILL)))
    }

    /// Asks the guest to power off and waits up to `timeout`, then kills.
    async fn graceful(&self, timeout: Duration) -> Result<ExitStatus> {
        if let Some(status) = self.exited() {
            return Ok(status);
        }
        if let Err(error) = self.vm.request_stop() {
            tracing::debug!(vm = %self.record.id, "stop request refused: {error}");
        }
        let deadline = Instant::now() + timeout;
        let mut poll = Duration::from_millis(1);
        loop {
            if let Some(status) = stopped_status(self.vm.state()) {
                return Ok(self.exit(status));
            }
            if Instant::now() >= deadline {
                return self.kill().await;
            }
            tokio::time::sleep(poll).await;
            poll = (poll * 2).min(Duration::from_millis(50));
        }
    }
}

impl Drop for VzHandle {
    fn drop(&mut self) {
        self.watcher.abort();
        if self.inner.exited().is_some() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = inner.kill().await {
                        tracing::warn!(vm = %inner.record.id, "stop on drop failed: {error}");
                    }
                });
            }
            // Without a runtime the stop cannot run; the helper process
            // dies with this one.
            Err(_) => {
                tracing::warn!(vm = %inner.record.id, "dropped outside a runtime; not stopped");
            }
        }
    }
}

#[async_trait]
impl VmHandle for VzHandle {
    fn id(&self) -> &VmId {
        &self.inner.record.id
    }

    fn record(&self) -> VmRecord {
        self.inner.record.clone()
    }

    fn state(&self) -> VmState {
        self.inner
            .exited()
            .map_or(VmState::Running, VmState::Exited)
    }

    fn events(&self) -> broadcast::Receiver<VmEvent> {
        self.inner.events.subscribe()
    }

    async fn shutdown(&self, mode: ShutdownMode) -> Result<ExitStatus> {
        match mode {
            ShutdownMode::Kill => self.inner.kill().await,
            ShutdownMode::Graceful { timeout } => self.inner.graceful(timeout).await,
        }
    }

    fn vsock(&self) -> Option<&dyn Vsock> {
        self.inner.vsock.then_some(self)
    }

    fn balloon(&self) -> Option<&dyn Balloon> {
        self.inner.balloon.then_some(self)
    }

    fn console(&self) -> Option<&dyn Console> {
        self.inner.console.as_ref().map(|c| c as &dyn Console)
    }

    fn agent_log(&self) -> Option<&dyn Console> {
        self.inner.agent_log.as_ref().map(|c| c as &dyn Console)
    }

    fn take_port_forward(&self) -> Option<Box<dyn PortForward>> {
        let manager = lock(&self.inner.net).as_mut()?.inbound.take()?;
        Some(Box::new(InboundForward::new(manager)))
    }

    fn debug(&self) -> Option<&dyn DebugSnapshot> {
        Some(self)
    }
}

#[async_trait]
impl Vsock for VzHandle {
    async fn dial(&self, port: u32) -> Result<VsockConn> {
        self.inner.require_running()?;
        let inner = Arc::clone(&self.inner);
        let fd = tokio::task::spawn_blocking(move || -> Result<OwnedFd> {
            let device = inner
                .vm
                .socket_devices()
                .into_iter()
                .next()
                .ok_or_else(|| driver_error("vsock", std::io::Error::other("no vsock device")))?;
            let connection = device
                .connect_blocking(port, DIAL_TIMEOUT)
                .map_err(|e| driver_error(format!("vsock dial port {port}"), e))?;
            // SAFETY: `into_raw_fd` hands over the connection's descriptor,
            // which nothing else owns from here on.
            Ok(unsafe { OwnedFd::from_raw_fd(connection.into_raw_fd()) })
        })
        .await
        .map_err(|e| driver_error("vsock dial task", e))??;
        Ok(VsockConn {
            fd,
            mode: IoMode::Async,
        })
    }
}

#[async_trait]
impl Balloon for VzHandle {
    async fn set_target(&self, bytes: u64) -> Result<()> {
        self.inner.require_running()?;
        let balloon =
            self.inner.vm.first_balloon_device().ok_or_else(|| {
                driver_error("balloon", std::io::Error::other("no balloon device"))
            })?;
        balloon.set_target_memory_size(bytes);
        Ok(())
    }

    async fn stats(&self) -> Result<BalloonStats> {
        self.inner.require_running()?;
        let target_bytes = self
            .inner
            .vm
            .first_balloon_device()
            .map_or(0, |balloon| balloon.target_memory_size());
        Ok(BalloonStats {
            target_bytes,
            current_bytes: None,
        })
    }
}

impl DebugSnapshot for VzHandle {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "driver": NAME,
            "id": self.inner.record.id.as_str(),
            "state": self.state().to_string(),
            "vz_state": format!("{:?}", self.inner.vm.state()),
            "net": lock(&self.inner.net).as_ref().map(NetResources::snapshot),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means a panic elsewhere; the data is still the
    // last consistent value written under it.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
