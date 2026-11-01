//! The port's `VmHandle` over a running [`Vmm`].
//!
//! A watcher thread blocks on the machine's PSCI SYSTEM_OFF channel, so an
//! idle VM costs no wakeups. SYSTEM_OFF tears the machine down and records
//! the one `VmEvent::Exited`. A SYSTEM_RESET only emits
//! `VmEvent::ResetRequested`: the orchestrator answers with `shutdown` and a
//! fresh `boot` from the same spec. Any other end of the vCPU threads that
//! the host did not ask for is a failure, `Exited` with code 1.
//!
//! A checkpoint that resumes the guest replaces the machine with one
//! restored from that checkpoint (the capture leaves the vCPUs stopped for
//! good). Each machine gets a generation, and a watcher whose machine was
//! replaced stands down.
//!
//! The machine is always stopped before it drops, also on the last
//! reference's drop: its io workers hold guest memory (ABX-415).

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use arcbox_vm_driver::{
    AfterCheckpoint, Balloon, BalloonStats, Checkpoint, CheckpointImage, CheckpointKind,
    CheckpointOptions, DebugSnapshot, Error, ExitStatus, IoMode, PortForward, Result, ShutdownMode,
    VmEvent, VmHandle, VmId, VmRecord, VmSpec, VmState, Vsock, VsockConn,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

use super::port_forward::{Forwards, InboundForward, SharedForwards};
use super::{Env, driver_error, image};
use crate::vmm::Vmm;

/// A running HV VM. Dropping it stops the VM unless it already exited.
pub struct HvHandle {
    inner: Arc<Inner>,
}

struct Inner {
    record: VmRecord,
    /// What the running machine was built from; a checkpoint records it.
    spec: VmSpec,
    env: Env,
    /// `None` once the machine has been stopped and dropped.
    vmm: Mutex<Option<Vmm>>,
    /// Bumped whenever the machine is replaced or stopped on request; a
    /// watcher only acts for the generation it was started for.
    generation: AtomicU64,
    state: Mutex<VmState>,
    /// One checkpoint at a time.
    checkpointing: tokio::sync::Mutex<()>,
    events: broadcast::Sender<VmEvent>,
    forwards: Option<SharedForwards>,
    forward_taken: AtomicBool,
}

impl HvHandle {
    /// Takes ownership of a started `vmm`, built from `spec`.
    pub(super) fn new(mut vmm: Vmm, spec: VmSpec, env: Env, record: VmRecord) -> Self {
        let (events, _) = broadcast::channel(16);
        let forwards = vmm.take_inbound_listener_manager().map(Forwards::new);
        let poweroff = vmm.hv_poweroff_receiver();
        let inner = Arc::new(Inner {
            record,
            spec,
            env,
            vmm: Mutex::new(Some(vmm)),
            generation: AtomicU64::new(0),
            state: Mutex::new(VmState::Running),
            checkpointing: tokio::sync::Mutex::new(()),
            events,
            forwards,
            forward_taken: AtomicBool::new(false),
        });
        inner.spawn_watcher(0, poweroff);
        Self { inner }
    }
}

/// Turns the end of the vCPU threads of machine `generation` into the VM's
/// event.
fn watch(inner: &Weak<Inner>, generation: u64, poweroff: &crossbeam_channel::Receiver<()>) {
    let powered_off = poweroff.recv().is_ok();
    let Some(inner) = inner.upgrade() else { return };
    if inner.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    if !powered_off && inner.with_vmm(Vmm::reset_requested) == Some(true) {
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
    if let Err(e) = inner.stop_vmm() {
        tracing::warn!(vm = %inner.record.id, "teardown after guest exit failed: {e}");
    }
    inner.exit(status);
}

impl Inner {
    fn spawn_watcher(
        self: &Arc<Self>,
        generation: u64,
        poweroff: Option<crossbeam_channel::Receiver<()>>,
    ) {
        let Some(poweroff) = poweroff else {
            tracing::error!(vm = %self.record.id, "started machine has no poweroff channel");
            return;
        };
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hv-watch".into())
            .spawn(move || watch(&weak, generation, &poweroff));
        if let Err(e) = spawned {
            tracing::error!(vm = %self.record.id, "watcher thread failed to start: {e}");
        }
    }

    fn state(&self) -> VmState {
        *lock(&self.state)
    }

    fn with_vmm<T>(&self, f: impl FnOnce(&Vmm) -> T) -> Option<T> {
        lock(&self.vmm).as_ref().map(f)
    }

    /// Stops and drops the machine. A second call does nothing. The
    /// generation moves first, so the watcher does not read the vCPU
    /// threads' exit as the guest's.
    fn stop_vmm(&self) -> Result<()> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let Some(mut vmm) = lock(&self.vmm).take() else {
            return Ok(());
        };
        vmm.stop().map_err(|e| driver_error("stop", e))
    }

    /// Records the exit once; later calls return the first status.
    fn exit(&self, status: ExitStatus) -> ExitStatus {
        {
            let mut state = lock(&self.state);
            if let VmState::Exited(earlier) = *state {
                return earlier;
            }
            *state = VmState::Exited(status);
        }
        if let (Some(forwards), Ok(runtime)) =
            (&self.forwards, tokio::runtime::Handle::try_current())
        {
            let forwards = Arc::clone(forwards);
            runtime.spawn(async move { forwards.lock().await.close().await });
        }
        let _ = self.events.send(VmEvent::Exited(status));
        tracing::info!(vm = %self.record.id, %status, "HV VM exited");
        status
    }

    fn require(&self, expected: VmState, name: &'static str) -> Result<()> {
        match self.state() {
            state if state == expected => Ok(()),
            state => Err(Error::WrongState {
                id: self.record.id.clone(),
                state,
                expected: name,
            }),
        }
    }

    /// Stops the VM without waiting for the guest.
    async fn kill(self: &Arc<Self>) -> Result<ExitStatus> {
        if let VmState::Exited(status) = self.state() {
            return Ok(status);
        }
        let inner = Arc::clone(self);
        tokio::task::spawn_blocking(move || inner.stop_vmm())
            .await
            .map_err(|e| driver_error("stop task", e))??;
        Ok(self.exit(ExitStatus::signaled(libc::SIGKILL)))
    }

    /// Waits up to `timeout` for the guest to power off, then kills it.
    async fn graceful(self: &Arc<Self>, timeout: std::time::Duration) -> Result<ExitStatus> {
        let mut events = self.events.subscribe();
        if let VmState::Exited(status) = self.state() {
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

    /// Captures the machine into `dst`. The capture leaves its vCPUs
    /// stopped for good, so `Resume` continues the guest in a machine
    /// restored from the image just written; host connections into the
    /// guest are reset by that, as by any restore. A capture that fails
    /// leaves no machine to continue: the VM is stopped and exits with 1.
    async fn checkpoint(
        self: &Arc<Self>,
        dst: &Path,
        opts: CheckpointOptions,
    ) -> Result<CheckpointImage> {
        if opts.kind == CheckpointKind::Diff {
            return Err(Error::InvalidSpec(format!(
                "{}: incremental checkpoints are not available",
                super::NAME
            )));
        }
        let _one = self.checkpointing.lock().await;
        self.require(VmState::Running, "running")?;
        let inner = Arc::clone(self);
        let dir = dst.to_path_buf();
        let captured = tokio::task::spawn_blocking(move || inner.capture(&dir))
            .await
            .map_err(|e| driver_error("checkpoint task", e))?;
        if let Err(e) = captured {
            let inner = Arc::clone(self);
            let _ = tokio::task::spawn_blocking(move || inner.stop_vmm()).await;
            self.exit(ExitStatus::exited(1));
            return Err(e);
        }
        match opts.after {
            AfterCheckpoint::HoldQuiesced => *lock(&self.state) = VmState::Quiesced,
            AfterCheckpoint::Resume => self.continue_from(dst).await?,
        }
        Ok(image(dst))
    }

    fn capture(&self, dir: &Path) -> Result<()> {
        let mut vmm = lock(&self.vmm);
        let vmm = vmm
            .as_mut()
            .ok_or_else(|| driver_error("checkpoint", std::io::Error::other("machine stopped")))?;
        let summary = vmm
            .hv_checkpoint(dir)
            .map_err(|e| driver_error("checkpoint", e))?;
        super::write_spec(dir, &self.spec)?;
        tracing::info!(vm = %self.record.id, "HV checkpoint: {summary}");
        Ok(())
    }

    /// Replaces the captured machine with one restored from `dir`, on the
    /// same disks.
    async fn continue_from(self: &Arc<Self>, dir: &Path) -> Result<()> {
        let inner = Arc::clone(self);
        let dir = dir.to_path_buf();
        let restored = tokio::task::spawn_blocking(move || -> Result<Vmm> {
            inner.stop_vmm()?;
            super::restore_vmm(&inner.spec, &inner.env, &dir)
        })
        .await
        .map_err(|e| driver_error("resume task", e))?;
        let mut vmm = match restored {
            Ok(vmm) => vmm,
            Err(e) => {
                self.exit(ExitStatus::exited(1));
                return Err(e);
            }
        };
        let manager = vmm.take_inbound_listener_manager();
        let poweroff = vmm.hv_poweroff_receiver();
        let generation = {
            let mut slot = lock(&self.vmm);
            *slot = Some(vmm);
            self.generation.load(Ordering::SeqCst)
        };
        self.spawn_watcher(generation, poweroff);
        if let Some(forwards) = &self.forwards {
            forwards.lock().await.rehome(manager).await;
        }
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if lock(&self.vmm).is_none() {
            return;
        }
        if let Err(e) = self.stop_vmm() {
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
        self.inner.state()
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
        self.inner
            .spec
            .vsock
            .is_some()
            .then_some(self as &dyn Vsock)
    }

    fn checkpoint(&self) -> Option<&dyn Checkpoint> {
        Some(self)
    }

    fn balloon(&self) -> Option<&dyn Balloon> {
        self.inner.spec.balloon.then_some(self as &dyn Balloon)
    }

    fn take_port_forward(&self) -> Option<Box<dyn PortForward>> {
        let forwards = self.inner.forwards.as_ref()?;
        if self.inner.forward_taken.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(Box::new(InboundForward::new(Arc::clone(forwards))))
    }

    fn debug(&self) -> Option<&dyn DebugSnapshot> {
        Some(self)
    }
}

#[async_trait]
impl Checkpoint for HvHandle {
    async fn checkpoint(&self, dst: &Path, opts: CheckpointOptions) -> Result<CheckpointImage> {
        self.inner.checkpoint(dst, opts).await
    }
}

#[async_trait]
impl Vsock for HvHandle {
    /// Returns as soon as the request is queued for the guest. A guest
    /// without a listener answers with a reset, which the caller reads as
    /// EOF on the returned socket; the dial itself never reports
    /// `ConnectionRefused`.
    async fn dial(&self, port: u32) -> Result<VsockConn> {
        self.inner.require(VmState::Running, "running")?;
        let inner = Arc::clone(&self.inner);
        // The dial blocks until a vCPU has queued the request.
        let raw = tokio::task::spawn_blocking(move || inner.with_vmm(|m| m.connect_vsock(port)))
            .await
            .map_err(|e| driver_error("vsock task", e))?
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
        self.inner.require(VmState::Running, "running")?;
        self.inner
            .with_vmm(|m| m.set_balloon_target(bytes))
            .ok_or_else(|| driver_error("balloon", std::io::Error::other("machine stopped")))?
            .map_err(|e| driver_error("balloon target", e))
    }

    async fn stats(&self) -> Result<BalloonStats> {
        let stats = self.inner.with_vmm(Vmm::get_balloon_stats);
        Ok(BalloonStats {
            target_bytes: stats.as_ref().map_or(0, |s| s.target_bytes),
            current_bytes: stats.map(|s| s.current_bytes),
        })
    }
}

impl DebugSnapshot for HvHandle {
    fn snapshot(&self) -> serde_json::Value {
        let snapshot = self.inner.with_vmm(Vmm::debug_snapshot).unwrap_or_default();
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null)
    }
}
