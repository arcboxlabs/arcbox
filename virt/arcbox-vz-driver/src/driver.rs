//! [`VzDriver`]: the port's `VmDriver` over Virtualization.framework.

use std::path::Path;
use std::time::{Duration, Instant};

use arcbox_net::darwin::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::{
    Adopt, CheckpointImage, DriverCapabilities, NestedVirt, RestoreSpec, Result, VmDriver,
    VmHandle, VmRecord, VmSpec,
};
use arcbox_vz::{GenericPlatform, SessionState, VZError, VirtualMachine, VirtualMachineState};
use async_trait::async_trait;

use crate::checkpoint::{Manifest, STATE_FILE, session_locked};
use crate::handle::{Parts, VzHandle};
use crate::net::HostServices;
use crate::render::{Rendered, render};
use crate::{NAME, driver_error};

/// How long a started VM may take to report `Running`.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Boots specs into Virtualization.framework VMs in this process.
pub struct VzDriver {
    host: HostServices,
}

impl VzDriver {
    /// A driver whose `UserNat` NICs share `dns` with the host DNS service
    /// and tunnel guest egress through `proxy`. Without `dns` each VM's
    /// forwarder keeps its own names; without `proxy` guest flows connect
    /// directly from the host.
    pub fn new(dns: Option<SharedDns>, proxy: Option<ProxyEnvironment>) -> Self {
        Self {
            host: HostServices { dns, proxy },
        }
    }

    /// Why a checkpoint cannot be taken right now, or `None` when it can.
    ///
    /// A VZ save needs the console user's session unlocked (ADR 0011,
    /// decision 6), so a runtime asks before it schedules one — an idle
    /// pause, a template capture — rather than pausing a guest only to
    /// have the save refused. A checkpoint asked for anyway fails with
    /// `Error::Unavailable` naming the same reason.
    pub fn checkpoint_blocker(&self) -> Option<SessionState> {
        let session = SessionState::current();
        (!session.allows_checkpoint()).then_some(session)
    }

    /// Builds the VM a rendered spec describes and starts it, from
    /// `state` when it restores a checkpoint.
    async fn run(
        &self,
        spec: VmSpec,
        rendered: Rendered,
        state: Option<&Path>,
        runtime_dir: &Path,
    ) -> Result<Box<dyn VmHandle>> {
        let Rendered {
            config,
            net,
            mut console,
            mut agent_log,
            machine_identifier,
        } = rendered;
        let vm = config.build().map_err(|e| driver_error("build", e))?;
        match state {
            None => vm.start().await.map_err(|e| driver_error("start", e))?,
            Some(state) => {
                vm.restore_state(state).await.map_err(|e| match e {
                    VZError::SessionLocked { state, .. } => session_locked(state),
                    e => driver_error("restore state", e),
                })?;
                vm.resume().await.map_err(|e| driver_error("resume", e))?;
            }
        }
        wait_running(&vm).await?;
        // VZ's helper holds its own copies of the console pipes now.
        for console in [&mut console, &mut agent_log].into_iter().flatten() {
            console.release_guest_ends();
        }
        tracing::info!(vm = %spec.id, cpus = spec.cpus, memory_mib = spec.memory_mib,
            restored = state.is_some(), "VZ VM running");
        Ok(Box::new(VzHandle::new(
            vm,
            Parts {
                record: VmRecord {
                    id: spec.id.clone(),
                    driver: NAME.to_owned(),
                    runtime_dir: runtime_dir.to_path_buf(),
                    process: None,
                },
                console,
                agent_log,
                net,
                vsock: spec.vsock.is_some(),
                balloon: spec.balloon,
                machine_identifier,
                spec,
            },
        )))
    }
}

#[async_trait]
impl VmDriver for VzDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            vsock: true,
            balloon: true,
            console: true,
            agent_log: true,
            port_forward: true,
            debug: true,
            checkpoint: true,
            nested_virt: if GenericPlatform::is_nested_virt_supported() {
                NestedVirt::supported()
            } else {
                NestedVirt::unsupported("this Mac does not support nested virtualization")
            },
            ..DriverCapabilities::default()
        }
    }

    async fn boot(&self, spec: VmSpec, runtime_dir: &Path) -> Result<Box<dyn VmHandle>> {
        let rendered = render(&spec, &self.host, None)?;
        self.run(spec, rendered, None, runtime_dir).await
    }

    /// Restores a `vz/v1` image into a new VM: the saved configuration
    /// with the restore's id, NICs and disks, and the saved machine
    /// identifier, which VZ requires. An image from another macOS build is
    /// refused with `PreconditionFailed` before VZ is asked.
    async fn restore(
        &self,
        image: &CheckpointImage,
        restore: RestoreSpec,
        runtime_dir: &Path,
    ) -> Result<Box<dyn VmHandle>> {
        let manifest = Manifest::read(image)?;
        let spec = manifest.restore_spec(restore)?;
        let machine_identifier = manifest.machine_identifier()?;
        let rendered = render(&spec, &self.host, Some(&machine_identifier))?;
        let state = image.dir.join(STATE_FILE);
        self.run(spec, rendered, Some(&state), runtime_dir).await
    }

    fn adopt(&self) -> Option<&dyn Adopt> {
        None
    }
}

/// Waits until a started VM reports `Running`.
async fn wait_running(vm: &VirtualMachine) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut poll = Duration::from_millis(1);
    loop {
        match vm.state() {
            VirtualMachineState::Running => return Ok(()),
            VirtualMachineState::Error | VirtualMachineState::Stopped => {
                return Err(driver_error(
                    "start",
                    std::io::Error::other(format!("VM stopped while starting ({:?})", vm.state())),
                ));
            }
            state if Instant::now() >= deadline => {
                return Err(driver_error(
                    "start",
                    std::io::Error::other(format!("VM still {state:?} after {START_TIMEOUT:?}")),
                ));
            }
            _ => {}
        }
        tokio::time::sleep(poll).await;
        poll = (poll * 2).min(Duration::from_millis(50));
    }
}
