//! `arcbox-host-net` — the macOS guest network for VMs that run on the host.
//!
//! The adapter of [`arcbox_vm_driver`]'s `GuestNetwork` port for host
//! sandboxes (ADR 0010), the macOS counterpart of `arcbox-tap-net`.
//!
//! # What lives here
//!
//! - [`Pool`] — the subnet leases are taken from.
//! - [`DnsPublisher`] — the composer's callback that publishes a live
//!   lease's name.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

mod dns;
mod pool;

pub use dns::{DnsPublisher, NoDns};
pub use pool::Pool;
