//! HV checkpoint spike: save a running Hypervisor.framework VM in one
//! process and resume it in another.
//!
//! ```text
//! checkpoint_spike save    <dir> [run_ms]            # boot, run, checkpoint into <dir>
//! checkpoint_spike restore <dir> [run_ms] [--eager]  # resume <dir>, run, verify
//! ```
//!
//! The guest (`guest.S`, prebuilt as `guest.bin`) idles in WFI on a 1 kHz
//! virtual-timer interrupt delivered by the in-kernel GICv3 and checks
//! register invariants on every wakeup, so a resumed guest that keeps
//! ticking without tripping `FAIL_MMIO` proves that vCPU, SIMD, timer, GIC
//! and memory state all came back. A checkpoint is three files: `state.json`
//! (vCPU and ICC registers, timer, counters), `gic.bin` (Apple's opaque
//! GIC blob) and `mem.bin` (sparse guest RAM). Restore maps `mem.bin`
//! `MAP_PRIVATE` by default, so pages fault in lazily; `--eager` reads it
//! into anonymous memory instead.
//!
//! Knobs: `SPIKE_RAM_MB` / `SPIKE_FILL_MB` (save) size guest RAM and
//! pre-dirty part of it; `SPIKE_SKIP=gic|icc|simd|vtimer|sysfirst`
//! (restore) leaves one part out as a negative control.
//!
//! Hypervisor.framework allows one VM per process, which is why save and
//! restore are separate invocations. The binary needs the hypervisor
//! entitlement: `codesign --force --entitlements bundle/arcbox.dev.entitlements -s - <bin>`.
//!
//! Method and results: `docs/experiments/2026-10-31-hv-checkpoint-spike.md`.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[path = "../checkpoint_common/mod.rs"]
#[allow(
    dead_code,
    reason = "shared with checkpoint_blk_spike, which uses other parts"
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
