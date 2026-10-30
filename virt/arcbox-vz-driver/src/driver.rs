//! [`VzDriver`]: the port's `VmDriver` over Virtualization.framework.

use std::path::Path;
use std::time::{Duration, Instant};

use arcbox_net::darwin::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::{
    Adopt, CheckpointImage, DriverCapabilities, Error, NestedVirt, RestoreSpec, Result, VmDriver,
    VmHandle, VmRecord, VmSpec,
};
use arcbox_vz::{GenericPlatform, VirtualMachine, VirtualMachineState};
use async_trait::async_trait;

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
            nested_virt: if GenericPlatform::is_nested_virt_supported() {
                NestedVirt::supported()
            } else {
                NestedVirt::unsupported("this Mac does not support nested virtualization")
            },
            ..DriverCapabilities::default()
        }
    }

    async fn boot(&self, spec: VmSpec, runtime_dir: &Path) -> Result<Box<dyn VmHandle>> {
        let Rendered {
            config,
            net,
            mut console,
            mut agent_log,
        } = render(&spec, &self.host)?;
        let vm = config.build().map_err(|e| driver_error("build", e))?;
        vm.start().await.map_err(|e| driver_error("start", e))?;
        wait_running(&vm).await?;
        // VZ's helper holds its own copies of the console pipes now.
        for console in [&mut console, &mut agent_log].into_iter().flatten() {
            console.release_guest_ends();
        }
        tracing::info!(vm = %spec.id, cpus = spec.cpus, memory_mib = spec.memory_mib, "VZ VM running");
        Ok(Box::new(VzHandle::new(
            vm,
            Parts {
                record: VmRecord {
                    id: spec.id,
                    driver: NAME.to_owned(),
                    runtime_dir: runtime_dir.to_path_buf(),
                    process: None,
                },
                console,
                agent_log,
                net,
                vsock: spec.vsock.is_some(),
                balloon: spec.balloon,
            },
        )))
    }

    /// This driver writes no checkpoints yet, so every image is foreign.
    async fn restore(
        &self,
        image: &CheckpointImage,
        _spec: RestoreSpec,
        _runtime_dir: &Path,
    ) -> Result<Box<dyn VmHandle>> {
        Err(Error::ForeignCheckpoint(image.format.clone()))
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
