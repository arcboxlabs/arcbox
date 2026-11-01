//! The seam through which a live lease's DNS name is published.

use std::net::IpAddr;

use arcbox_vm_driver::{Result, VmId};
use async_trait::async_trait;

/// Publishes and withdraws a VM's DNS name; the composer supplies it.
///
/// The network calls [`register`](Self::register) when a lease goes live
/// (activation and adoption) and [`deregister`](Self::deregister) when it
/// stops being live (quarantine and release). What the name is —
/// `<id>.sandbox.arcbox.local` for a sandbox — and which registry holds it
/// are the composer's: the daemon's implementation forwards to
/// `Runtime::register_dns` / `deregister_dns`. The address is the lease's
/// pool address.
#[async_trait]
pub trait DnsPublisher: Send + Sync {
    /// Points `vm`'s name at `address`, replacing whatever it pointed at.
    ///
    /// A failure fails the activation or adoption that asked for it, which
    /// the network rolls back.
    async fn register(&self, vm: &VmId, address: IpAddr) -> Result<()>;

    /// Withdraws `vm`'s name. Withdrawing a name that is not published does
    /// nothing; there is nothing a caller could do about a failure, so the
    /// implementation logs it.
    async fn deregister(&self, vm: &VmId);
}

/// A publisher for composers that publish no names: a test, or a host
/// without a DNS service.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDns;

#[async_trait]
impl DnsPublisher for NoDns {
    async fn register(&self, _vm: &VmId, _address: IpAddr) -> Result<()> {
        Ok(())
    }

    async fn deregister(&self, _vm: &VmId) {}
}
