//! [`HvMachineDriver`]: the `arcbox-vm-driver` port over this crate's
//! Hypervisor.framework machine, in the calling process.
//!
//! Hypervisor.framework allows one VM per process, and the daemon's process
//! holds the System VM, so nothing in the daemon boots through this driver.
//! It is the engine of `arcbox-hv-driver`'s per-VM process
//! (`arcbox-hv-vmm`), which serves the same port to the daemon over a
//! socket (ADR 0010).
//!
//! - `render` — [`VmSpec`] → [`VmmConfig`](crate::VmmConfig), pure and
//!   tested.
//! - `handle` — the port's `VmHandle` over a running [`Vmm`]: exit and reset
//!   events from a watcher thread, shutdown, and the vsock, balloon, debug
//!   and port-forward capabilities.
//! - `port_forward` — `PortForward` over the NAT datapath's inbound
//!   listener manager.
//!
//! Checkpoint is not wired yet, so every image is foreign. The guest
//! console stays inside the machine, so there is no `Console`.

use std::path::Path;

use arcbox_fakeip::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::{
    Adopt, CheckpointImage, DriverCapabilities, Error, NestedVirt, RestoreSpec, Result, VmDriver,
    VmHandle, VmRecord, VmSpec,
};
use async_trait::async_trait;

use crate::vmm::Vmm;

mod handle;
mod port_forward;
pub mod render;

use handle::HvHandle;

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = "hv";

/// What every machine the driver builds shares with the host.
#[derive(Clone, Default)]
pub(crate) struct Env {
    dns: Option<SharedDns>,
    proxy: Option<ProxyEnvironment>,
}

/// Boots specs into Hypervisor.framework machines in this process.
pub struct HvMachineDriver {
    env: Env,
}

impl HvMachineDriver {
    /// A driver whose `UserNat` datapaths share `dns` with the host DNS
    /// service and tunnel guest egress through `proxy`. Without `dns` each
    /// VM's forwarder keeps its own names; without `proxy` guest flows
    /// connect directly from the host.
    pub const fn new(dns: Option<SharedDns>, proxy: Option<ProxyEnvironment>) -> Self {
        Self {
            env: Env { dns, proxy },
        }
    }
}

#[async_trait]
impl VmDriver for HvMachineDriver {
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
        let mut vmm = new_vmm(&spec, &self.env)?;
        // Building guest memory, loading the kernel and spawning the vCPU
        // threads block; keep them off the async workers.
        let vmm = tokio::task::spawn_blocking(move || -> Result<Vmm> {
            vmm.start().map_err(|e| driver_error("boot", e))?;
            Ok(vmm)
        })
        .await
        .map_err(|e| driver_error("boot task", e))??;
        tracing::info!(vm = %spec.id, cpus = spec.cpus, memory_mib = spec.memory_mib, "HV VM running");
        let record = record(&spec, runtime_dir);
        Ok(Box::new(HvHandle::new(vmm, spec, self.env.clone(), record)))
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

/// An unstarted machine for `spec`.
fn new_vmm(spec: &VmSpec, env: &Env) -> Result<Vmm> {
    let mut vmm = Vmm::new(render::vmm_config(spec)?).map_err(|e| driver_error("create", e))?;
    if let Some(dns) = &env.dns {
        vmm.set_shared_dns(dns.clone());
    }
    if let Some(proxy) = &env.proxy {
        vmm.set_proxy_env(proxy.clone());
    }
    Ok(vmm)
}

fn record(spec: &VmSpec, runtime_dir: &Path) -> VmRecord {
    VmRecord {
        id: spec.id.clone(),
        driver: NAME.to_owned(),
        runtime_dir: runtime_dir.to_path_buf(),
        process: None,
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
