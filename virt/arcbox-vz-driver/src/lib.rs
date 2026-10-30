//! `arcbox-vz-driver` — the Virtualization.framework adapter for the VM
//! driver port.
//!
//! [`arcbox_vm_driver`] is the vocabulary of "a VM on this host"; this crate
//! makes a [`VmSpec`](arcbox_vm_driver::VmSpec) run under Apple's
//! Virtualization.framework through [`arcbox_vz`]. VZ is a managed VMM: the
//! VM runs in Apple's helper process and this crate holds it in-process, so
//! there is no adopt, prepare or staging, and checkpointing waits for the
//! shim to bind `saveMachineStateTo` (vm-stack-redesign "Deferred by
//! decision").
//!
//! # What lives here
//!
//! - [`driver`] — [`VzDriver`], the port's `VmDriver`: validates and renders
//!   a spec, builds and starts the VM.
//! - `render` — [`VmSpec`](arcbox_vm_driver::VmSpec) → a VZ configuration
//!   plus the host resources the VM keeps for its life. Device order is
//!   bus order, so the guest sees disks and NICs in spec order.
//! - `net` — the NIC attachments: the userspace NAT datapath
//!   (`UserNat`, from `arcbox_net::darwin::guest_nat`), the host NAT
//!   (`HostNat`, vmnet shared mode or Apple NAT), and a caller's socket
//!   (`FileHandle`).
//! - `console` — the console and agent-log channels over VZ's pipes.
//! - `handle` — the port's `VmHandle` over a running VM: state and exit
//!   from a watcher, shutdown, and the vsock, balloon, console, debug and
//!   port-forward capabilities.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

mod console;
pub mod driver;
mod handle;
mod net;
mod port_forward;
mod render;

pub use driver::VzDriver;

use arcbox_vm_driver::Error;

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = "vz";

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
