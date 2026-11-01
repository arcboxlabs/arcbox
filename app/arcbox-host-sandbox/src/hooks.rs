//! The host location's half of the Sandbox API.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use arcbox_computer_runtime::RootfsBuilder;
use arcbox_computer_runtime::workload::{self, ImageRef};
use arcbox_constants::dns::Domain;
use arcbox_sandbox_service::docker::DockerEngine;
use arcbox_sandbox_service::{ImageOrigin, SandboxHooks};
use async_trait::async_trait;

use crate::HostSandboxOptions;

/// Names are published by the network when a lease goes live, images come
/// from their registry and then from the System VM's dockerd (D5), and
/// Dockerfile templates are built by that dockerd.
pub struct HostHooks {
    domain: Domain,
    docker: DockerEngine,
    docker_ready: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Where image layouts exported out of dockerd are staged.
    layouts: PathBuf,
}

impl HostHooks {
    pub fn new(options: &HostSandboxOptions, layouts: PathBuf) -> Self {
        Self {
            domain: options.domain.clone(),
            docker: DockerEngine::new(&options.docker_socket),
            docker_ready: Arc::clone(&options.system_docker_ready),
            layouts,
        }
    }

    /// Exports `image` out of the System VM's dockerd and converts it.
    async fn export(
        &self,
        image: &str,
        rootfs: &RootfsBuilder,
        pinned: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<String> {
        if !(self.docker_ready)() {
            anyhow::bail!("the System VM is not running, so its dockerd cannot export {image}");
        }
        let layout = self.docker.export_layout(image, &self.layouts).await?;
        Ok(rootfs.convert_layer_to_rootfs(&layout, pinned).await?)
    }
}

#[async_trait]
impl SandboxHooks for HostHooks {
    fn dns_domain(&self) -> &Domain {
        &self.domain
    }

    /// Nothing to do: host sandbox names are not published until the
    /// lease pool is routed (see `POOL`), so they answer NODATA.
    fn register_dns(&self, _id: &str, _ip: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Nothing to do: nothing was published.
    fn deregister_dns(&self, _id: &str) {}

    /// The registry first; when it does not have the image, the System
    /// VM's dockerd (D5) — the common case for an image an agent just
    /// built. A local build skips the registry.
    async fn image_rootfs(
        &self,
        image: &str,
        origin: ImageOrigin,
        rootfs: &RootfsBuilder,
        pinned: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<String> {
        if origin == ImageOrigin::LocalBuild {
            return self.export(image, rootfs, pinned).await;
        }
        let reference = ImageRef::parse(image)?;
        let pull = match workload::pull(&reference).await {
            Ok(pulled) => {
                return Ok(rootfs
                    .convert_image_to_rootfs(pulled.source, &pulled.manifest_digest, pinned)
                    .await?);
            }
            Err(error) => error,
        };
        tracing::info!(%reference, error = %pull, "registry pull failed; trying the System VM's dockerd");
        self.export(image, rootfs, pinned)
            .await
            .map_err(|export| anyhow::anyhow!("{pull}; {export:#}"))
    }

    async fn build_image(&self, dockerfile: &[u8], tag: &str) -> anyhow::Result<()> {
        if !(self.docker_ready)() {
            anyhow::bail!(
                "building a Dockerfile template needs the System VM's dockerd, which is not running"
            );
        }
        if self.docker.inspect_image(tag).await.is_ok() {
            tracing::info!(%tag, "reusing previously built dockerfile image");
            return Ok(());
        }
        self.docker.build_image(dockerfile, tag).await
    }
}
