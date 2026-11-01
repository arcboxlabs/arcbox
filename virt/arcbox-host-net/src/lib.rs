//! `arcbox-host-net` — the macOS guest network for VMs that run on the host.
//!
//! The adapter of [`arcbox_vm_driver`]'s `GuestNetwork` port for host
//! sandboxes (ADR 0010), the macOS counterpart of `arcbox-tap-net`. Each VM
//! gets its own userspace NAT datapath (`arcbox_net::darwin::guest_nat`),
//! so every guest boots on the same fixed link and no two guests share one
//! (D1); the lease's pool address names the VM on the host and is published
//! through the composer's [`DnsPublisher`].
//!
//! # What lives here
//!
//! - [`HostNetwork`] — the port's `GuestNetwork` and `NetworkReconcile`:
//!   the address ledger, the quarantine protocol, and a `UserNat` NIC per
//!   lease whose datapath the driver builds in the VMM's process.
//! - [`Pool`] — the subnet leases are taken from.
//! - [`DnsPublisher`] — the composer's callback that publishes a live
//!   lease's name.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

mod dns;
mod network;
mod pool;

pub use dns::{DnsPublisher, NoDns};
pub use network::{HostNetwork, NIC_ID};
pub use pool::Pool;
