//! `arcbox-hv-driver` — the Hypervisor.framework adapter for the VM driver
//! port, one VMM process per VM.
//!
//! Hypervisor.framework allows one VM per process, and the daemon already
//! holds the System VM, so every VM this driver runs lives in its own
//! `arcbox-hv-vmm` process (ADR 0010, "Process model").
//!
//! - [`serve`] — the process side, run by the `arcbox-hv-vmm` binary.
//! - [`proto`] — the control protocol's requests, responses and events.
//! - [`wire`] — framing on the control socket, with `SCM_RIGHTS`.
//! - [`process`] — a VMM process as the driver sees it: exit, kill, events.
//! - [`control`] — one request on the control socket, with a deadline.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

pub mod control;
pub mod process;
pub mod proto;
#[doc(hidden)]
pub mod serve;
pub mod wire;

use arcbox_vm_driver::Error;

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = arcbox_vmm::hv_driver::NAME;

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
