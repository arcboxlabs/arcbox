//! Publishing a host sandbox's DNS name.

use std::net::IpAddr;
use std::sync::Arc;

use arcbox_host_net::DnsPublisher;
use arcbox_vm_driver::VmId;
use async_trait::async_trait;

/// Where a live host sandbox's name is published: the daemon's sandbox
/// DNS records, the same ones the nested location's sandboxes get.
#[async_trait]
pub trait SandboxNames: Send + Sync {
    /// Points sandbox `id`'s name at `address`.
    ///
    /// # Errors
    /// When the host refuses the record.
    async fn register(&self, id: &str, address: IpAddr) -> anyhow::Result<()>;

    /// Withdraws sandbox `id`'s name; withdrawing an absent one does nothing.
    async fn deregister(&self, id: &str);
}

/// The network's [`DnsPublisher`] over the daemon's [`SandboxNames`]. A VM
/// id is the sandbox id.
pub struct Publisher(Arc<dyn SandboxNames>);

impl Publisher {
    pub fn new(names: Arc<dyn SandboxNames>) -> Self {
        Self(names)
    }
}

#[async_trait]
impl DnsPublisher for Publisher {
    async fn register(&self, vm: &VmId, address: IpAddr) -> arcbox_vm_driver::Result<()> {
        self.0
            .register(vm.as_str(), address)
            .await
            .map_err(|error| arcbox_vm_driver::Error::Network(format!("publish {vm}: {error:#}")))
    }

    async fn deregister(&self, vm: &VmId) {
        self.0.deregister(vm.as_str()).await;
    }
}
