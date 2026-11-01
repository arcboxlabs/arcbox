//! A spawned `arcbox-hv-vmm` process with no VM yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arcbox_vm_driver::{
    CheckpointImage, Error, ExitStatus, PreparedVm, RestoreSpec, Result, VmHandle, VmId, VmRecord,
    VmSpec, VmState,
};
use async_trait::async_trait;

use crate::control::{self, Control, unexpected};
use crate::handle::HvHandle;
use crate::process::VmmProcess;
use crate::proto::{Request, Response};

/// A VMM process waiting for its spec. Dropping it kills the process
/// unless a boot or restore handed it to a handle.
pub struct HvPrepared {
    record: VmRecord,
    process: Arc<VmmProcess>,
    control: Control,
    /// Set once a handle owns the process.
    handed_over: AtomicBool,
}

impl HvPrepared {
    pub(crate) fn new(record: VmRecord, process: Arc<VmmProcess>) -> Self {
        let control = Control::new(process.socket().to_path_buf());
        Self {
            record,
            process,
            control,
            handed_over: AtomicBool::new(false),
        }
    }

    fn require_alive(&self) -> Result<()> {
        match self.process.exit_status() {
            None => Ok(()),
            Some(status) => Err(Error::WrongState {
                id: self.record.id.clone(),
                state: VmState::Exited(status),
                expected: "prepared",
            }),
        }
    }

    async fn start(&self, request: Request) -> Result<Box<dyn VmHandle>> {
        self.require_alive()?;
        let booted = match self.control.call(request, control::SLOW).await? {
            (Response::Booted(booted), _) => booted,
            (other, _) => return Err(unexpected(&other)),
        };
        self.handed_over.store(true, Ordering::SeqCst);
        Ok(Box::new(HvHandle::new(
            self.record.clone(),
            Arc::clone(&self.process),
            booted,
        )))
    }
}

impl Drop for HvPrepared {
    fn drop(&mut self) {
        if !self.handed_over.load(Ordering::SeqCst) {
            self.process.signal_kill();
        }
    }
}

#[async_trait]
impl PreparedVm for HvPrepared {
    fn id(&self) -> &VmId {
        &self.record.id
    }

    fn record(&self) -> VmRecord {
        self.record.clone()
    }

    fn alive(&self) -> bool {
        self.process.alive()
    }

    async fn boot(&self, spec: VmSpec) -> Result<Box<dyn VmHandle>> {
        if spec.id != self.record.id {
            return Err(Error::InvalidSpec(format!(
                "spec {} for a VM prepared as {}",
                spec.id, self.record.id
            )));
        }
        spec.validate()?;
        self.start(Request::Boot(Box::new(spec))).await
    }

    async fn restore(
        &self,
        image: &CheckpointImage,
        spec: RestoreSpec,
    ) -> Result<Box<dyn VmHandle>> {
        crate::own_format(image)?;
        if spec.id != self.record.id {
            return Err(Error::InvalidSpec(format!(
                "restore {} for a VM prepared as {}",
                spec.id, self.record.id
            )));
        }
        self.start(Request::Restore {
            image: image.clone(),
            spec,
        })
        .await
    }

    async fn discard(&self) -> Result<ExitStatus> {
        self.process.kill().await
    }
}
