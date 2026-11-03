//! macOS QEMU TCG backend: the System VM in a `qemu-system-aarch64` child,
//! for Macs without hardware virtualization.

#[allow(
    dead_code,
    reason = "the QEMU machine renders it from the next commits on"
)]
mod args;
mod process;
#[allow(
    dead_code,
    reason = "the QEMU machine drives it from the next commits on"
)]
mod qmp;

pub use process::reap_orphaned_qemu;
