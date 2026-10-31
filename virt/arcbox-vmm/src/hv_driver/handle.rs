//! The port's `VmHandle` over a running [`HvMachine`].
//!
//! A watcher thread blocks on the machine's PSCI SYSTEM_OFF channel, so an
//! idle VM costs no wakeups. SYSTEM_OFF tears the machine down and records
//! the one `VmEvent::Exited`. A SYSTEM_RESET only emits
//! `VmEvent::ResetRequested`: the orchestrator answers with `shutdown` and a
//! fresh `boot` from the same spec. Any other end of the vCPU threads that
//! the host did not ask for is a failure, `Exited` with code 1.
//!
//! The machine is always stopped before it drops, also on the last
//! reference's drop: its io workers hold guest memory (ABX-415).

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use arcbox_net::darwin::inbound_relay::InboundListenerManager;
use arcbox_vm_driver::{
    Balloon, BalloonStats, DebugSnapshot, Error, ExitStatus, IoMode, PortForward, Result,
    ShutdownMode, VmEvent, VmHandle, VmId, VmRecord, VmState, Vsock, VsockConn,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

use super::driver_error;
use super::port_forward::InboundForward;
use crate::vcpu_stats::VmDebugSnapshot;
use crate::vmm::darwin_hv::HvMachine;

/// A running HV VM. Dropping it stops the VM unless it already exited.
pub struct HvHandle {
    inner: Arc<Inner>,
}

struct Inner {
    record: VmRecord,
    /// `None` once the machine has been stopped and dropped.
    machine: Mutex<Option<HvMachine>>,
    exit: Mutex<Option<ExitStatus>>,
    /// Set before a host-initiated stop, so the watcher does not read the
    /// vCPU threads' exit as a guest failure.
    stopping: AtomicBool,
    events: broadcast::Sender<VmEvent>,
    port_forward: Mutex<Option<InboundListenerManager>>,
    vsock: bool,
    balloon: bool,
}

/// What a running VM keeps besides the machine itself.
pub struct Parts {
    pub record: VmRecord,
    pub vsock: bool,
    pub balloon: bool,
}

impl HvHandle {
    /// Takes ownership of a started `machine`.
    pub fn new(mut machine: HvMachine, parts: Parts) -> Self {
        let (events, _) = broadcast::channel(16);
        let poweroff = machine.poweroff_receiver();
        let port_forward = machine.take_inbound_listener_manager();
        let inner = Arc::new(Inner {
            record: parts.record,
            machine: Mutex::new(Some(machine)),
            exit: Mutex::new(None),
            stopping: AtomicBool::new(false),
            events,
            port_forward: Mutex::new(port_forward),
            vsock: parts.vsock,
            balloon: parts.balloon,
        });
        match poweroff {
            Some(poweroff) => {
                let weak = Arc::downgrade(&inner);
                let spawned = std::thread::Builder::new()
                    .name("hv-watch".into())
                    .spawn(move || watch(&weak, &poweroff));
                if let Err(e) = spawned {
                    tracing::error!(vm = %inner.record.id, "watcher thread failed to start: {e}");
                }
            }
            None => {
                tracing::error!(vm = %inner.record.id, "started machine has no poweroff channel");
            }
        }
        Self { inner }
    }
}

/// Turns the end of the vCPU threads into the VM's event.
fn watch(inner: &Weak<Inner>, poweroff: &crossbeam_channel::Receiver<()>) {
    let powered_off = poweroff.recv().is_ok();
    let Some(inner) = inner.upgrade() else { return };
    if inner.stopping.load(Ordering::SeqCst) {
        return;
    }
    if !powered_off && inner.with_machine(HvMachine::reset_requested) == Some(true) {
        tracing::info!(vm = %inner.record.id, "guest requested a reset");
        // A send error only means nobody is subscribed.
        let _ = inner.events.send(VmEvent::ResetRequested);
        return;
    }
    let status = if powered_off {
        ExitStatus::exited(0)
    } else {
        tracing::error!(vm = %inner.record.id, "vCPU threads exited without PSCI SYSTEM_OFF");
        ExitStatus::exited(1)
    };
    inner.stopping.store(true, Ordering::SeqCst);
    if let Err(e) = inner.stop_machine() {
        tracing::warn!(vm = %inner.record.id, "teardown after guest exit failed: {e}");
    }
    inner.exit(status);
}

impl Inner {
    fn exited(&self) -> Option<ExitStatus> {
        *lock(&self.exit)
    }

    fn with_machine<T>(&self, f: impl FnOnce(&HvMachine) -> T) -> Option<T> {
        lock(&self.machine).as_ref().map(f)
    }

    /// Stops and drops the machine. A second call does nothing.
    fn stop_machine(&self) -> Result<()> {
        let Some(mut machine) = lock(&self.machine).take() else {
            return Ok(());
        };
        machine.stop().map_err(|e| driver_error("stop", e))
    }

    /// Records the exit once; later calls return the first status.
    fn exit(&self, status: ExitStatus) -> ExitStatus {
        {
            let mut exit = lock(&self.exit);
            if let Some(earlier) = *exit {
                return earlier;
            }
            *exit = Some(status);
        }
        drop(lock(&self.port_forward).take());
        let _ = self.events.send(VmEvent::Exited(status));
        tracing::info!(vm = %self.record.id, %status, "HV VM exited");
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

    /// Stops the VM without waiting for the guest.
    async fn kill(self: &Arc<Self>) -> Result<ExitStatus> {
        if let Some(status) = self.exited() {
            return Ok(status);
        }
        self.stopping.store(true, Ordering::SeqCst);
        let inner = Arc::clone(self);
        tokio::task::spawn_blocking(move || inner.stop_machine())
            .await
            .map_err(|e| driver_error("stop task", e))??;
        Ok(self.exit(ExitStatus::signaled(libc::SIGKILL)))
    }

    /// Waits up to `timeout` for the guest to power off, then kills it.
    async fn graceful(self: &Arc<Self>, timeout: std::time::Duration) -> Result<ExitStatus> {
        let mut events = self.events.subscribe();
        if let Some(status) = self.exited() {
            return Ok(status);
        }
        let exited = tokio::time::timeout(timeout, async {
            loop {
                match events.recv().await {
                    Ok(VmEvent::Exited(status)) => return Some(status),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .await;
        match exited {
            Ok(Some(status)) => Ok(status),
            _ => self.kill().await,
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if lock(&self.machine).is_none() {
            return;
        }
        self.stopping.store(true, Ordering::SeqCst);
        if let Err(e) = self.stop_machine() {
            tracing::warn!(vm = %self.record.id, "stop on drop failed: {e}");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl VmHandle for HvHandle {
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

    /// `Graceful` waits for the guest's own power-off: Hypervisor.framework
    /// has no host-side power button, so the caller asks the guest (through
    /// its agent) before it calls this.
    async fn shutdown(&self, mode: ShutdownMode) -> Result<ExitStatus> {
        match mode {
            ShutdownMode::Kill => self.inner.kill().await,
            ShutdownMode::Graceful { timeout } => self.inner.graceful(timeout).await,
        }
    }

    fn vsock(&self) -> Option<&dyn Vsock> {
        self.inner.vsock.then_some(self as &dyn Vsock)
    }

    fn balloon(&self) -> Option<&dyn Balloon> {
        self.inner.balloon.then_some(self as &dyn Balloon)
    }

    fn take_port_forward(&self) -> Option<Box<dyn PortForward>> {
        lock(&self.inner.port_forward)
            .take()
            .map(|manager| Box::new(InboundForward::new(manager)) as Box<dyn PortForward>)
    }

    fn debug(&self) -> Option<&dyn DebugSnapshot> {
        Some(self)
    }
}

#[async_trait]
impl Vsock for HvHandle {
    /// Returns as soon as the request is queued for the guest. A guest
    /// without a listener answers with a reset, which the caller reads as
    /// EOF on the returned socket; the dial itself never reports
    /// `ConnectionRefused`.
    async fn dial(&self, port: u32) -> Result<VsockConn> {
        self.inner.require_running()?;
        let raw = self
            .inner
            .with_machine(|m| m.connect_vsock(port))
            .ok_or_else(|| driver_error("vsock", std::io::Error::other("machine stopped")))?
            .map_err(|e| driver_error(format!("vsock dial port {port}"), e))?;
        // SAFETY: `connect_vsock` hands over a fresh descriptor that nothing
        // else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(VsockConn {
            fd,
            mode: IoMode::Blocking,
        })
    }
}

#[async_trait]
impl Balloon for HvHandle {
    async fn set_target(&self, bytes: u64) -> Result<()> {
        self.inner.require_running()?;
        self.inner
            .with_machine(|m| m.set_balloon_target(bytes))
            .ok_or_else(|| driver_error("balloon", std::io::Error::other("machine stopped")))?
            .map_err(|e| driver_error("balloon target", e))
    }

    async fn stats(&self) -> Result<BalloonStats> {
        let (target_bytes, current_bytes) = self
            .inner
            .with_machine(HvMachine::balloon_bytes)
            .unwrap_or_default();
        Ok(BalloonStats {
            target_bytes,
            current_bytes: Some(current_bytes),
        })
    }
}

impl DebugSnapshot for HvHandle {
    fn snapshot(&self) -> serde_json::Value {
        let snapshot = self
            .inner
            .with_machine(|m| VmDebugSnapshot {
                devices: m.virtio_debug(),
                vcpus: m.vcpu_snapshots(),
                kick_broadcasts: 0,
                unpark_broadcasts: 0,
            })
            .unwrap_or_default();
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null)
    }
}
