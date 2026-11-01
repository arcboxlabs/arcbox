//! The port's `VmHandle` over a VM in an `arcbox-hv-vmm` process.
//!
//! The VM's life is the process's: `Exited` is the process's exit, a kill
//! is `SIGKILL` to it. Everything else is a request on its control socket.
//! Dropping the handle kills the process unless it was detached.

use std::sync::{Arc, Mutex, MutexGuard};

use arcbox_vm_driver::{
    Detach, Error, ExitStatus, IoMode, Result, ShutdownMode, VmEvent, VmHandle, VmId, VmRecord,
    VmState, Vsock, VsockConn,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::control::{self, Control, unexpected};
use crate::process::VmmProcess;
use crate::proto::{Booted, Request, Response};

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

    fn detach(&self) -> Option<&dyn Detach> {
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
impl Detach for HvHandle {
    async fn detach(&self) -> Result<VmRecord> {
        self.inner.process.detach();
        Ok(self.inner.record.clone())
    }
}
