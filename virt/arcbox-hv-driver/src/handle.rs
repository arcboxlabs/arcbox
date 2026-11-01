//! The port's `VmHandle` over a VM in an `arcbox-hv-vmm` process.
//!
//! The VM's life is the process's: `Exited` is the process's exit, a kill
//! is `SIGKILL` to it. Everything else is a request on its control socket.
//! Dropping the handle kills the process unless it was detached.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arcbox_vm_driver::{
    AfterCheckpoint, Balloon, BalloonStats, Checkpoint, CheckpointImage, CheckpointKind,
    CheckpointOptions, Console, DebugSnapshot, Detach, Error, ExitStatus, ForwardRule, IoMode,
    PortForward, Result, ShutdownMode, VmEvent, VmHandle, VmId, VmRecord, VmState, Vsock,
    VsockConn,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::control::{self, Control, unexpected};
use crate::process::VmmProcess;
use crate::proto::{Booted, Request, Response};
use crate::{driver_error, set_nonblocking};

/// A VM running in its own VMM process.
pub struct HvHandle {
    inner: Arc<Inner>,
}

struct Inner {
    record: VmRecord,
    process: Arc<VmmProcess>,
    control: Control,
    booted: Booted,
    /// `Running` or `Quiesced` while the process lives.
    state: Mutex<VmState>,
    forward_taken: AtomicBool,
    /// The console pipe's read end, once asked for.
    console: Mutex<Option<OwnedFd>>,
}

impl HvHandle {
    pub(crate) fn new(record: VmRecord, process: Arc<VmmProcess>, booted: Booted) -> Self {
        let control = Control::new(process.socket().to_path_buf());
        let state = match booted.state {
            VmState::Quiesced => VmState::Quiesced,
            _ => VmState::Running,
        };
        Self {
            inner: Arc::new(Inner {
                record,
                process,
                control,
                booted,
                state: Mutex::new(state),
                forward_taken: AtomicBool::new(false),
                console: Mutex::new(None),
            }),
        }
    }
}

impl Inner {
    fn state(&self) -> VmState {
        self.process
            .exit_status()
            .map_or_else(|| *lock(&self.state), VmState::Exited)
    }

    fn require_running(&self) -> Result<()> {
        match self.state() {
            VmState::Running => Ok(()),
            state => Err(Error::WrongState {
                id: self.record.id.clone(),
                state,
                expected: "running",
            }),
        }
    }

    /// The console pipe, a duplicate of the one this handle keeps.
    fn console_fd(&self) -> Result<OwnedFd> {
        let mut slot = lock(&self.console);
        if let Some(fd) = slot.as_ref() {
            return Ok(fd.try_clone()?);
        }
        let fd = match self
            .control
            .call_blocking(&Request::Console, control::QUICK)?
        {
            (Response::Fd, Some(fd)) => fd,
            (other, _) => return Err(unexpected(&other)),
        };
        set_nonblocking(&fd)?;
        let copy = fd.try_clone()?;
        *slot = Some(fd);
        Ok(copy)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.process.signal_kill();
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
        self.inner.process.events()
    }

    /// `Graceful` waits for the guest's own power-off: Hypervisor.framework
    /// has no host-side power button, so the caller asks the guest (through
    /// its agent) before it calls this.
    async fn shutdown(&self, mode: ShutdownMode) -> Result<ExitStatus> {
        if let ShutdownMode::Graceful { timeout } = mode
            && let Some(status) = self.inner.process.wait(timeout).await
        {
            return Ok(status);
        }
        self.inner.process.kill().await
    }

    fn vsock(&self) -> Option<&dyn Vsock> {
        self.inner.booted.vsock.then_some(self as &dyn Vsock)
    }

    fn checkpoint(&self) -> Option<&dyn Checkpoint> {
        Some(self)
    }

    fn detach(&self) -> Option<&dyn Detach> {
        Some(self)
    }

    fn balloon(&self) -> Option<&dyn Balloon> {
        self.inner.booted.balloon.then_some(self as &dyn Balloon)
    }

    fn console(&self) -> Option<&dyn Console> {
        self.inner.booted.console.then_some(self as &dyn Console)
    }

    fn take_port_forward(&self) -> Option<Box<dyn PortForward>> {
        if !self.inner.booted.port_forward || self.inner.forward_taken.swap(true, Ordering::SeqCst)
        {
            return None;
        }
        Some(Box::new(RemoteForward {
            control: self.inner.control.clone(),
        }))
    }

    fn debug(&self) -> Option<&dyn DebugSnapshot> {
        Some(self)
    }
}

#[async_trait]
impl Vsock for HvHandle {
    /// The descriptor is one end of the VMM process's socketpair into the
    /// guest's vsock device, passed over the control socket. Like the
    /// in-process machine, a guest without a listener answers with a
    /// reset, which reads as EOF.
    async fn dial(&self, port: u32) -> Result<VsockConn> {
        self.inner.require_running()?;
        match self
            .inner
            .control
            .call(Request::Dial(port), control::DIAL)
            .await?
        {
            (Response::Fd, Some(fd)) => Ok(VsockConn {
                fd,
                mode: IoMode::Blocking,
            }),
            (other, _) => Err(unexpected(&other)),
        }
    }
}

#[async_trait]
impl Checkpoint for HvHandle {
    /// See `arcbox_vmm::hv_driver`: `Resume` continues the guest in a
    /// machine restored in the same process, so host connections into it
    /// are reset.
    async fn checkpoint(&self, dst: &Path, opts: CheckpointOptions) -> Result<CheckpointImage> {
        if opts.kind == CheckpointKind::Diff {
            return Err(Error::InvalidSpec(
                "hv: incremental checkpoints are not available".into(),
            ));
        }
        self.inner.require_running()?;
        let hold = opts.after == AfterCheckpoint::HoldQuiesced;
        let request = Request::Checkpoint {
            dir: dst.to_path_buf(),
            hold,
        };
        let image = match self.inner.control.call(request, control::SLOW).await? {
            (Response::Image(image), _) => image,
            (other, _) => return Err(unexpected(&other)),
        };
        if hold {
            *lock(&self.inner.state) = VmState::Quiesced;
        }
        Ok(image)
    }
}

#[async_trait]
impl Detach for HvHandle {
    async fn detach(&self) -> Result<VmRecord> {
        self.inner.process.detach();
        Ok(self.inner.record.clone())
    }
}

#[async_trait]
impl Balloon for HvHandle {
    async fn set_target(&self, bytes: u64) -> Result<()> {
        self.inner.require_running()?;
        match self
            .inner
            .control
            .call(Request::BalloonTarget(bytes), control::QUICK)
            .await?
        {
            (Response::Ok, _) => Ok(()),
            (other, _) => Err(unexpected(&other)),
        }
    }

    async fn stats(&self) -> Result<BalloonStats> {
        match self
            .inner
            .control
            .call(Request::BalloonStats, control::QUICK)
            .await?
        {
            (Response::Balloon { target, current }, _) => Ok(BalloonStats {
                target_bytes: target,
                current_bytes: current,
            }),
            (other, _) => Err(unexpected(&other)),
        }
    }
}

#[async_trait]
impl Console for HvHandle {
    /// What the console pipe holds now, up to `max_bytes`; empty when the
    /// guest wrote nothing since, and after the VM exited.
    async fn read_output(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut pipe = std::fs::File::from(inner.console_fd()?);
            let mut out = vec![0u8; max_bytes];
            let mut got = 0;
            while got < max_bytes {
                match pipe.read(&mut out[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            out.truncate(got);
            Ok(out)
        })
        .await
        .map_err(|e| driver_error("console task", e))?
    }

    fn dup_reader(&self) -> Result<OwnedFd> {
        self.inner.console_fd()
    }
}

impl DebugSnapshot for HvHandle {
    fn snapshot(&self) -> serde_json::Value {
        match self
            .inner
            .control
            .call_blocking(&Request::Debug, control::QUICK)
        {
            Ok((Response::Debug(value), _)) => value,
            Ok(_) | Err(_) => serde_json::Value::Null,
        }
    }
}

/// Port forwarding served by the VMM process: its listeners are bound
/// there and relay into its datapath.
struct RemoteForward {
    control: Control,
}

#[async_trait]
impl PortForward for RemoteForward {
    async fn add(&mut self, rule: ForwardRule) -> Result<u16> {
        match self
            .control
            .call(Request::ForwardAdd(rule.into()), control::QUICK)
            .await?
        {
            (Response::Port(port), _) => Ok(port),
            (other, _) => Err(unexpected(&other)),
        }
    }

    async fn remove(&mut self, rule: ForwardRule) {
        if let Err(e) = self
            .control
            .call(Request::ForwardRemove(rule.into()), control::QUICK)
            .await
        {
            tracing::debug!(?rule, "removing a forward: {e}");
        }
    }

    async fn clear(&mut self) {
        if let Err(e) = self
            .control
            .call(Request::ForwardClear, control::QUICK)
            .await
        {
            tracing::debug!("clearing forwards: {e}");
        }
    }
}
