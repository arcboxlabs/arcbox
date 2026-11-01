//! The host location's half of the Sandbox API.

use std::collections::BTreeSet;
use std::path::PathBuf;

use arcbox_computer_runtime::RootfsBuilder;
use arcbox_computer_runtime::workload::{self, ImageRef};
use arcbox_constants::dns::Domain;
use arcbox_sandbox_service::{ImageOrigin, SandboxHooks};
use async_trait::async_trait;

use crate::HostSandboxOptions;

/// Names are published by the network when a lease goes live, and images
/// come from their registry.
pub struct HostHooks {
    domain: Domain,
}

impl HostHooks {
    pub fn new(options: &HostSandboxOptions, _layouts: PathBuf) -> Self {
        Self {
            domain: options.domain.clone(),
        }
    }
}

#[async_trait]
impl SandboxHooks for HostHooks {
    fn dns_domain(&self) -> &Domain {
        &self.domain
    }

    /// Nothing to do: the network publishes the name when the lease goes
    /// live (`arcbox_host_net::DnsPublisher`), at the same address.
    fn register_dns(&self, _id: &str, _ip: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Nothing to do: the network withdraws the name when the lease stops
    /// being live.
    fn deregister_dns(&self, _id: &str) {}

    async fn image_rootfs(
        &self,
        image: &str,
        _origin: ImageOrigin,
        rootfs: &RootfsBuilder,
        pinned: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<String> {
        let pulled = workload::pull(&ImageRef::parse(image)?).await?;
        Ok(rootfs
            .convert_image_to_rootfs(pulled.source, &pulled.manifest_digest, pinned)
            .await?)
    }

    async fn build_image(&self, _dockerfile: &[u8], _tag: &str) -> anyhow::Result<()> {
        anyhow::bail!("Dockerfile templates are not built on the host location yet")
    }
}
