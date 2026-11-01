//! HV checkpoint spike 3: a real Linux guest checkpointed in the middle of
//! block I/O by the production HV VMM, and resumed in another process.
//!
//! ```text
//! checkpoint_linux_spike save    <dir> <run_s>   # boot, run, checkpoint into <dir>
//! checkpoint_linux_spike restore <dir> <run_s>   # resume <dir>, run, verify
//! ```
//!
//! The guest is the ArcBox kernel from the boot-asset cache booting an
//! initramfs whose `/init` (`guest/init.c`, built by `guest/build.sh`)
//! runs four I/O threads, two on `/dev/vda` (virtio-blk) and two on
//! `/dev/arcboxhvc0` (the HVC fast path), each verifying every read against
//! the seq numbers it keeps in RAM, and prints a `tick` line a second on
//! hvc0. `save` checkpoints through `Vmm::hv_checkpoint`; `restore` clones
//! the saved disk, resumes through `Vmm::hv_restore` and passes when ticks,
//! the I/O counters of all four threads and the guest clock keep advancing
//! with no verify failure.
//!
//! Environment: `SPIKE_KERNEL` (default `~/.arcbox/boot/<newest>/kernel`),
//! `SPIKE_INITRD` (default `<dir>/../initramfs.cpio.gz`), `SPIKE_VCPUS`
//! (save, default 4), `SPIKE_APPEND` (save: extra kernel parameters, e.g.
//! `maxcpus=2 spike_online_at=8` to checkpoint with CPUs off and online them
//! after the restore), `SPIKE_NET=1` (save: add virtio-net and vsock, keep a
//! guest TCP connection to a host echo server and a host vsock connection to
//! a guest echo server busy, and check after the restore that both stale
//! connections end at once and new ones work; spike 4),
//! `SPIKE_RESAVE=<dir2>` (restore: checkpoint the restored machine into
//! `<dir2>` at the end of the run, a second generation that `restore`
//! resumes like any other). Every run also holds a balloon inflation and
//! sends console input across the checkpoint. Negative control:
//! `SPIKE_TAMPER=disk` (restore) resumes on an empty disk. The binary needs
//! the hypervisor entitlement: `codesign --force --entitlements bundle/arcbox.dev.entitlements -s - <bin>`.
//!
//! Method and results: `docs/experiments/2026-10-31-hv-checkpoint-linux-spike.md`,
//! `docs/experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md` and
//! `docs/experiments/2026-11-01-hv-checkpoint-gaps.md`.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod spike;

fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    if let Err(e) = spike::main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
