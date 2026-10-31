//! HV checkpoint spike 2: a multi-vCPU VM with a virtio-blk device under
//! load, saved in one process and resumed in another.
//!
//! ```text
//! checkpoint_blk_spike save    <dir> [run_ms]            # boot, run, checkpoint into <dir>
//! checkpoint_blk_spike restore <dir> [run_ms] [--eager]  # resume <dir>, run, verify
//! ```
//!
//! The guest (`guest/`, prebuilt as `guest.bin` by `guest/build.sh`) runs
//! on every vCPU: each takes 1 kHz virtual-timer interrupts and drives its
//! own queue of one ArcBox `VirtioBlock`, writing 4 KiB blocks stamped with
//! a sequence number and reading other blocks back to check them against
//! the sequence numbers it keeps in RAM. A resumed guest that keeps
//! ticking and completing I/O without tripping `FAIL_MMIO` proves that the
//! vCPUs, the GIC, guest RAM, the device's transport state and the disk
//! all came back mutually consistent.
//!
//! The host side is the production device path: `DeviceManager` decodes
//! the MMIO, `blk_io_worker_loop` threads do the I/O, and the SPI goes
//! through `IrqChip` into the in-kernel GIC. A checkpoint is `vcpuN.json`,
//! `gic.bin`, `mmio.txt` (the device's `VirtioMmioState`), `mem.bin` and
//! `disk.img`, an APFS clone of the disk taken after the I/O workers have
//! drained. Restore clones `disk.img` again, so a checkpoint can be resumed
//! any number of times.
//!
//! Knobs: `SPIKE_VCPUS` (save, default 4). Negative controls:
//! `SPIKE_SKIP=mmio|disk|gic|icc` (restore) leaves one part out;
//! `SPIKE_LATE_GIC=1` (save) reads the GIC after the vCPUs are destroyed.
//!
//! The binary needs the hypervisor entitlement:
//! `codesign --force --entitlements bundle/arcbox.dev.entitlements -s - <bin>`.
//!
//! Method and results: `docs/experiments/2026-10-31-hv-checkpoint-blk-spike.md`.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[path = "../checkpoint_common/mod.rs"]
#[allow(
    dead_code,
    reason = "shared with checkpoint_spike, which uses other parts"
)]
mod common;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod spike;

fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    if let Err(e) = spike::main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
