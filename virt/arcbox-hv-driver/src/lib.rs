//! `arcbox-hv-driver` — the Hypervisor.framework adapter for the VM driver
//! port, one VMM process per VM.
//!
//! Hypervisor.framework allows one VM per process, and the daemon already
//! holds the System VM, so every VM this driver runs lives in its own
//! `arcbox-hv-vmm` process (ADR 0010, "Process model").
//!
//! - [`proto`] — the control protocol's requests, responses and events.
//! - [`wire`] — framing on the control socket, with `SCM_RIGHTS`.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

pub mod proto;
pub mod wire;

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = arcbox_vmm::hv_driver::NAME;
