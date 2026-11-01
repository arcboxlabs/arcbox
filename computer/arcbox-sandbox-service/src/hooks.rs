//! What a composer supplies besides the manager: the parts of the Sandbox
//! API that depend on where the service runs.

use std::collections::BTreeSet;
use std::path::PathBuf;

use arcbox_computer_runtime::RootfsBuilder;
use arcbox_connect::v1::{SandboxCleanupTicket, SandboxPortProtocol};
use arcbox_constants::dns::Domain;
use async_trait::async_trait;

use crate::{SandboxError, SandboxService};

/// Where an image [`SandboxHooks::image_rootfs`] converts comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOrigin {
    /// A reference a caller named (`docker:<ref>`, a template's
    /// `docker_ref`): it may be in a registry, in dockerd, or both.
    Reference,
    /// An image this service just built in dockerd ([`SandboxHooks::build_image`]);
    /// no registry has it.
    LocalBuild,
}

/// The composer's half of a [`SandboxService`].
///
/// The System VM's agent publishes sandbox names in its own DNS server,
/// exports `docker:<ref>` images from its own dockerd, and forwards ports
/// with guest DNAT. The daemon's host location publishes names through the
/// guest network's DNS seam, pulls images from their registry first and
/// falls back to the System VM's dockerd (D5), and publishes ports through
/// the VM driver instead of this protocol.
#[async_trait]
pub trait SandboxHooks: Send + Sync {
    /// The DNS domain sandbox ids must form a name under.
    fn dns_domain(&self) -> &Domain;

    /// Points `id`'s DNS name at `ip`. Called after a create, a restore
    /// and a resume hand back the sandbox's address.
    fn register_dns(&self, id: &str, ip: &str) -> anyhow::Result<()>;

    /// Withdraws `id`'s DNS name. Called after a stop, pause, remove and
    /// cleanup finalization.
    fn deregister_dns(&self, id: &str);

    /// The bootable ext4 rootfs for the container image `image`, built with
    /// `rootfs` so it carries `vm-agent`. `pinned` are images the
    /// conversion's cache sweep must keep.
    async fn image_rootfs(
        &self,
        image: &str,
        origin: ImageOrigin,
        rootfs: &RootfsBuilder,
        pinned: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<String>;

    /// Builds `dockerfile` into an image tagged `tag`, unless an image with
    /// that tag exists already. The tag is content-addressed, so an
    /// existing one is the same build.
    async fn build_image(&self, dockerfile: &[u8], tag: &str) -> anyhow::Result<()>;

    /// Forwards `sandbox_port` of sandbox `id`, answering the port the host
    /// side of the forward targets. Refused unless the composer forwards
    /// ports through this protocol.
    async fn forward_port(
        &self,
        _service: &SandboxService,
        _id: &str,
        _sandbox_port: u16,
        _protocol: SandboxPortProtocol,
    ) -> Result<u16, SandboxError> {
        Err(SandboxError::Unsupported(
            "this location does not forward ports through the agent protocol".into(),
        ))
    }

    /// Removes a forward [`Self::forward_port`] installed. Idempotent.
    async fn remove_port(
        &self,
        _service: &SandboxService,
        _id: &str,
        _sandbox_port: u16,
        _protocol: SandboxPortProtocol,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported(
            "this location does not forward ports through the agent protocol".into(),
        ))
    }

    /// Releases the composer's forwarding state for one validated cleanup
    /// generation, before the service finalizes it. `ip` is the address
    /// the generation held (unspecified for the startup ticket).
    async fn release_forwards(
        &self,
        _ticket: &SandboxCleanupTicket,
        _ip: std::net::Ipv4Addr,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
}
