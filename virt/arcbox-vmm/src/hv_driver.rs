//! [`HvDriver`]: the `arcbox-vm-driver` port over this crate's
//! Hypervisor.framework machine.
//!
//! - `render` — [`VmSpec`] → [`VmmConfig`](crate::VmmConfig), pure and
//!   tested.
//! - `handle` — the port's `VmHandle` over a running
//!   [`HvMachine`]: exit and reset events from a watcher thread, shutdown,
//!   and the vsock, balloon, debug and port-forward capabilities.
//! - `port_forward` — `PortForward` over the NAT datapath's inbound
//!   listener manager.
//!
//! Checkpoint is absent: the machine cannot capture a snapshot yet, so
//! there is nothing to put behind the capability (vm-stack-redesign, R4
//! seam map). The guest console stays inside the machine, so there is no
//! `Console` either.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use arcbox_fakeip::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::{
    Adopt, CheckpointImage, DriverCapabilities, Error, NestedVirt, RestoreSpec, Result, VmDriver,
    VmHandle, VmRecord, VmSpec,
};
use async_trait::async_trait;

use crate::vmm::darwin_hv::HvMachine;

mod handle;
mod port_forward;
pub mod render;

use handle::{HvHandle, Parts};

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = "hv";

/// Boots specs into Hypervisor.framework machines in this process.
pub struct HvDriver {
    dns: Option<SharedDns>,
    proxy: Option<ProxyEnvironment>,
}

impl HvDriver {
    /// A driver whose `UserNat` datapaths share `dns` with the host DNS
    /// service and tunnel guest egress through `proxy`. Without `dns` each
    /// VM's forwarder keeps its own names; without `proxy` guest flows
    /// connect directly from the host.
    pub const fn new(dns: Option<SharedDns>, proxy: Option<ProxyEnvironment>) -> Self {
        Self { dns, proxy }
    }
}

#[async_trait]
impl VmDriver for HvDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            vsock: true,
            balloon: true,
            port_forward: true,
            debug: true,
            nested_virt: NestedVirt::unsupported(
                "Hypervisor.framework guests cannot run their own hypervisor",
            ),
            ..DriverCapabilities::default()
        }
    }

    async fn boot(&self, spec: VmSpec, runtime_dir: &Path) -> Result<Box<dyn VmHandle>> {
        let config = render::vmm_config(&spec)?;
        let mut machine = HvMachine::new(config, Arc::new(AtomicBool::new(false)));
        if let Some(dns) = &self.dns {
            machine.set_shared_dns(dns.clone());
        }
        if let Some(proxy) = &self.proxy {
            machine.set_proxy_env(proxy.clone());
        }
        // Building guest memory, loading the kernel and spawning the vCPU
        // threads block; keep them off the async workers.
        let machine = tokio::task::spawn_blocking(move || -> Result<HvMachine> {
            let started = machine.initialize().and_then(|()| machine.start());
            if let Err(e) = started {
                // Initialize may have spawned workers that hold guest memory.
                if let Err(stop) = machine.stop() {
                    tracing::warn!("teardown after a failed boot failed: {stop}");
                }
                return Err(driver_error("boot", e));
            }
            Ok(machine)
        })
        .await
        .map_err(|e| driver_error("boot task", e))??;
        tracing::info!(vm = %spec.id, cpus = spec.cpus, memory_mib = spec.memory_mib, "HV VM running");
        Ok(Box::new(HvHandle::new(
            machine,
            Parts {
                record: VmRecord {
                    id: spec.id,
                    driver: NAME.to_owned(),
                    runtime_dir: runtime_dir.to_path_buf(),
                    process: None,
                },
                vsock: spec.vsock.is_some(),
                balloon: spec.balloon,
            },
        )))
    }

    /// This driver writes no checkpoints, so every image is foreign.
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

/// Folds a failure inside the adapter into the port's error, keeping the
/// original as its source.
fn driver_error(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Error {
    Error::Driver {
        driver: NAME,
        message: message.into(),
        source: Some(Box::new(source)),
    }
}

/// A spec this driver cannot run.
fn unsupported(what: &str) -> Error {
    Error::InvalidSpec(format!("{NAME}: {what}"))
}
