//! A prepared VM for a driver without `Prepare`: the spawn happens at boot.
//!
//! The port defines `VmDriver::boot(spec, dir)` as `prepare` followed by
//! `PreparedVm::boot`, so the converse holds too: a prepared VM can be the
//! remembered `(driver, id, isolation, dir)` that `boot` and `restore` hand
//! to the driver. What the flows need from the prepared half beyond that is
//! the kill — `discard` is how every release stops a VM — so the VM it
//! booted stays reachable from here. Weakly: the handle the boot returned
//! owns the VM, exactly as with a real prepared VM.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use arcbox_vm_driver::{
    Balloon, Checkpoint, CheckpointImage, Console, DebugSnapshot, Detach, Error, ExitStatus,
    IsolationSpec, PortForward, PreparedVm, RestoreSpec, Result, ShutdownMode, Staging, VmDriver,
    VmEvent, VmHandle, VmId, VmRecord, VmSpec, VmState, Vsock, VsockListen,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

/// What a discard reports for a VM that had already gone: it was killed by
/// its handle's drop, which is a SIGKILL as far as the port is concerned.
const SIGKILL: i32 = 9;

/// Where a deferred VM is in its one life.
enum Life {
    /// Nothing booted yet.
    Waiting,
    /// Booted or restored; the handle the caller holds owns the VM.
    Booted(Weak<dyn VmHandle>),
    /// Discarded: no boot may follow.
    Discarded,
}

/// A VM whose VMM starts at boot.
pub struct DeferredVm {
    driver: Arc<dyn VmDriver>,
    id: VmId,
    isolation: IsolationSpec,
    runtime_dir: PathBuf,
    life: Mutex<Life>,
}

impl DeferredVm {
    pub fn new(
        driver: Arc<dyn VmDriver>,
        id: VmId,
        isolation: IsolationSpec,
        runtime_dir: &Path,
    ) -> Self {
        Self {
            driver,
            id,
            isolation,
            runtime_dir: runtime_dir.to_path_buf(),
            life: Mutex::new(Life::Waiting),
        }
    }

    /// Refuse a second boot, a boot after a discard, and a spec for another
    /// VM — the rules `PreparedVm::boot` states.
    fn admit(&self, id: &VmId, isolation: &IsolationSpec) -> Result<()> {
        if *id != self.id || *isolation != self.isolation {
            return Err(Error::InvalidSpec(format!(
                "vm {} was prepared as {} under {:?}",
                id, self.id, self.isolation
            )));
        }
        match *self.life.lock().unwrap() {
            Life::Waiting => Ok(()),
            Life::Booted(_) | Life::Discarded => Err(Error::InvalidSpec(format!(
                "vm {} was already booted or discarded",
                self.id
            ))),
        }
    }

    /// Keep a weak grip on what the boot returned, and hand the caller the
    /// owning one. A discard that raced the boot kills what the boot made.
    async fn booted(&self, handle: Box<dyn VmHandle>) -> Result<Box<dyn VmHandle>> {
        let handle: Arc<dyn VmHandle> = Arc::from(handle);
        let discarded = {
            let mut life = self.life.lock().unwrap();
            if matches!(*life, Life::Discarded) {
                true
            } else {
                *life = Life::Booted(Arc::downgrade(&handle));
                false
            }
        };
        if discarded {
            handle.shutdown(ShutdownMode::Kill).await?;
            return Err(Error::InvalidSpec(format!(
                "vm {} was discarded while it booted",
                self.id
            )));
        }
        Ok(Box::new(SharedVm(handle)))
    }
}

#[async_trait]
impl PreparedVm for DeferredVm {
    fn id(&self) -> &VmId {
        &self.id
    }

    fn record(&self) -> VmRecord {
        VmRecord {
            id: self.id.clone(),
            driver: self.driver.name().to_owned(),
            runtime_dir: self.runtime_dir.clone(),
            process: None,
        }
    }

    fn alive(&self) -> bool {
        match &*self.life.lock().unwrap() {
            Life::Waiting => true,
            Life::Booted(vm) => vm
                .upgrade()
                .is_some_and(|vm| !matches!(vm.state(), VmState::Exited(_))),
            Life::Discarded => false,
        }
    }

    async fn boot(&self, spec: VmSpec) -> Result<Box<dyn VmHandle>> {
        self.admit(&spec.id, &spec.isolation)?;
        let handle = self.driver.boot(spec, &self.runtime_dir).await?;
        self.booted(handle).await
    }

    async fn restore(
        &self,
        image: &CheckpointImage,
        spec: RestoreSpec,
    ) -> Result<Box<dyn VmHandle>> {
        self.admit(&spec.id, &spec.isolation)?;
        let handle = self.driver.restore(image, spec, &self.runtime_dir).await?;
        self.booted(handle).await
    }

    async fn discard(&self) -> Result<ExitStatus> {
        let previous = std::mem::replace(&mut *self.life.lock().unwrap(), Life::Discarded);
        match previous {
            // Nothing ever ran.
            Life::Waiting => Ok(ExitStatus::exited(0)),
            Life::Booted(vm) => match vm.upgrade() {
                Some(vm) => vm.shutdown(ShutdownMode::Kill).await,
                None => Ok(ExitStatus::signaled(SIGKILL)),
            },
            Life::Discarded => Ok(ExitStatus::signaled(SIGKILL)),
        }
    }
}

/// The handle a deferred boot returns: the driver's, shared with the
/// [`DeferredVm`] that keeps a weak grip on it for the kill.
struct SharedVm(Arc<dyn VmHandle>);

#[async_trait]
impl VmHandle for SharedVm {
    fn id(&self) -> &VmId {
        self.0.id()
    }

    fn record(&self) -> VmRecord {
        self.0.record()
    }

    fn state(&self) -> VmState {
        self.0.state()
    }

    fn events(&self) -> broadcast::Receiver<VmEvent> {
        self.0.events()
    }

    async fn shutdown(&self, mode: ShutdownMode) -> Result<ExitStatus> {
        self.0.shutdown(mode).await
    }

    fn vsock(&self) -> Option<&dyn Vsock> {
        self.0.vsock()
    }

    fn checkpoint(&self) -> Option<&dyn Checkpoint> {
        self.0.checkpoint()
    }

    fn vsock_listener(&self) -> Option<&dyn VsockListen> {
        self.0.vsock_listener()
    }

    fn detach(&self) -> Option<&dyn Detach> {
        self.0.detach()
    }

    fn staging(&self) -> Option<&dyn Staging> {
        self.0.staging()
    }

    fn balloon(&self) -> Option<&dyn Balloon> {
        self.0.balloon()
    }

    fn console(&self) -> Option<&dyn Console> {
        self.0.console()
    }

    fn agent_log(&self) -> Option<&dyn Console> {
        self.0.agent_log()
    }

    fn take_port_forward(&self) -> Option<Box<dyn PortForward>> {
        self.0.take_port_forward()
    }

    fn debug(&self) -> Option<&dyn DebugSnapshot> {
        self.0.debug()
    }
}

#[cfg(test)]
mod tests {
    use arcbox_vm_driver::testkit::FakeDriver;
    use arcbox_vm_driver::{BootSpec, ConsoleSpec, DriverCapabilities, VmDriver as _};

    use super::*;

    fn spec(id: &VmId) -> VmSpec {
        VmSpec {
            id: id.clone(),
            cpus: 1,
            memory_mib: 128,
            boot: BootSpec::Kernel {
                image: "/vmlinux".into(),
                cmdline: String::new(),
                initrd: None,
            },
            disks: vec![],
            nics: vec![],
            vsock: None,
            shares: vec![],
            console: ConsoleSpec::Off,
            balloon: false,
            entropy: false,
            dirty_tracking: false,
            isolation: IsolationSpec::None,
            rosetta: false,
            nested_virt: false,
            debug_console: None,
        }
    }

    /// The prepared shape over a driver that has none: the boot goes to the
    /// driver, the discard kills what it booted, and the prepared VM admits
    /// one boot only.
    #[tokio::test]
    async fn a_deferred_vm_boots_through_the_driver_and_discard_kills_it() {
        let driver = FakeDriver::builder()
            .capabilities(DriverCapabilities {
                prepare: false,
                staging: false,
                ..FakeDriver::new().capabilities()
            })
            .build();
        let id = VmId::new("deferred").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prepared = DeferredVm::new(
            Arc::new(driver.clone()),
            id.clone(),
            IsolationSpec::None,
            dir.path(),
        );
        assert_eq!(prepared.record().process, None, "no process to journal");

        let handle = prepared.boot(spec(&id)).await.unwrap();
        assert_eq!(handle.state(), VmState::Running);
        assert!(prepared.alive());
        assert!(prepared.boot(spec(&id)).await.is_err(), "one boot only");

        prepared.discard().await.unwrap();
        assert_eq!(driver.shutdowns(&id), [ShutdownMode::Kill]);
        assert!(matches!(handle.state(), VmState::Exited(_)));
        assert!(!prepared.alive());
    }
}
