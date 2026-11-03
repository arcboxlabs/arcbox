//! macOS QEMU TCG backend: the System VM in a `qemu-system-aarch64` child,
//! for Macs without hardware virtualization.

mod process;

pub use process::reap_orphaned_qemu;
