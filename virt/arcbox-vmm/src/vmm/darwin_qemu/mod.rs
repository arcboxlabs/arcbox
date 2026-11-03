//! macOS QEMU TCG backend: the System VM in a `qemu-system-aarch64` child,
//! for Macs without hardware virtualization.

#[allow(
    dead_code,
    reason = "the QEMU machine renders it from the next commits on"
)]
mod args;
#[allow(
    dead_code,
    reason = "the QEMU machine spawns it from the next commit on"
)]
mod process;
#[allow(
    dead_code,
    reason = "the QEMU machine drives it from the next commits on"
)]
mod qmp;

pub use process::reap_orphaned_qemu;
