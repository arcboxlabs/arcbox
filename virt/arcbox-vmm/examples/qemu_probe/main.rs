//! Boots a probe initramfs on the QEMU TCG backend through the production
//! `Vmm`, the way the System VM boots, and reports what the guest saw.
//!
//! ```text
//! qemu_probe <mode> <work> [mem_mib] [vcpus]
//! ```
//!
//! `<work>` holds `initramfs.cpio.gz` (from `guest/build.sh`) and the two
//! data images, `docker.img` and `docker-meta.img`; pidfiles go to
//! `<work>/run`. Modes, each named after the guest's `probe=` mode:
//!
//! - `probe`: block order and discard, DHCP through the host network
//!   stack, DNS, HTTP, `hvc1`, then a guest power-off.
//! - `bench`, `docker`, `touch`: a CPU and syscall benchmark, dockerd to
//!   API readiness, and 3 GiB touched then freed; each ends in a power-off.
//! - `reset`: the guest reboots; the probe reboots the VMM in place.
//! - `stop`: the guest idles; the probe stops it (QMP `quit`).
//! - `orphan`: the guest idles and the probe sleeps, to be `kill -9`ed.
//! - `reap`: stops the QEMU an `orphan` run left behind.
//!
//! Environment: `ARCBOX_QEMU_PATH`, `QEMU_PROBE_KERNEL` and
//! `QEMU_PROBE_ROOTFS` (the boot-asset kernel and `rootfs.erofs`).
//! Results: `docs/logs/2026-11-02-qemu-tcg-host-backend.md`.

#[cfg(target_os = "macos")]
mod probe;

fn main() {
    #[cfg(target_os = "macos")]
    if let Err(e) = probe::main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("qemu_probe runs on macOS only");
}
