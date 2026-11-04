//! `arcbox-host-net` — the macOS guest network for VMs that run on the host.
//!
//! The adapter of [`arcbox_vm_driver`]'s `GuestNetwork` port for host
//! sandboxes (ADR 0011), the macOS counterpart of `arcbox-tap-net`. Each VM
//! gets its own userspace NAT datapath (`arcbox_net::darwin::guest_nat`),
//! so every guest boots on the same fixed link and no two guests share one
//! (D1); the lease's pool address names the VM on the host and is published
//! through the composer's [`DnsPublisher`].
//!
//! # What lives here
//!
//! - [`HostNetwork`] — the port's `GuestNetwork` and `NetworkReconcile`:
//!   the address ledger, the quarantine protocol, and per lease either a
//!   datapath of its own ([`DatapathPlacement::Network`], a
//!   `FileHandle` NIC, for VZ) or a `UserNat` NIC whose datapath the
//!   driver builds in the VMM's process ([`DatapathPlacement::Driver`],
//!   for HV).
//! - [`Pool`] — the subnet leases are taken from.
//! - [`DnsPublisher`] — the composer's callback that publishes a live
//!   lease's name.
//! - [`DatapathServices`] — what every datapath shares with the host: DNS
//!   ownership, the egress proxy, the NIC MTU.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

mod datapath;
mod dns;
mod network;
mod pool;
mod port_forward;

pub use datapath::{DEFAULT_MTU, DatapathServices};
pub use dns::{DnsPublisher, NoDns};
pub use network::{DatapathPlacement, HostNetwork, NIC_ID};
pub use pool::Pool;
