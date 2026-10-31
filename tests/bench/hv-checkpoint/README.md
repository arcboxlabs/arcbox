# HV checkpoint probes

Three examples in `virt/arcbox-vmm/examples/` checkpoint a
Hypervisor.framework VM in one process and resume it in another. The first
two build their own machine and share the vCPU, GIC and RAM code in
`checkpoint_common/`; the third drives the production HV machine through
`Vmm::hv_checkpoint` and `Vmm::hv_restore`.

| Script | Example | Guest | Write-up |
|---|---|---|---|
| `run.sh` | `checkpoint_spike` | one vCPU, timer only | `docs/experiments/2026-10-31-hv-checkpoint-spike.md` |
| `run-blk.sh` | `checkpoint_blk_spike` | 1–12 vCPUs, timer, ArcBox virtio-blk under load | `docs/experiments/2026-10-31-hv-checkpoint-blk-spike.md` |
| `run-linux.sh` | `checkpoint_linux_spike` | Linux (boot-asset kernel, initramfs) on 1–8 vCPUs, I/O on virtio-blk and the HVC block path | `docs/experiments/2026-10-31-hv-checkpoint-linux-spike.md` |
| `run-net.sh` | `checkpoint_linux_spike` (`SPIKE_NET=1`) | the `run-linux.sh` guest plus live TCP (echo, idle, download over virtio-net and the splicetcp datapath) and vsock connections | `docs/experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md`, `docs/experiments/2026-10-31-hv-checkpoint-tcp-reset-spike.md` |

```bash
tests/bench/hv-checkpoint/run.sh [work-dir]
tests/bench/hv-checkpoint/run-blk.sh [work-dir]
tests/bench/hv-checkpoint/run-linux.sh [work-dir]
tests/bench/hv-checkpoint/run-net.sh [work-dir]
```

Each script builds its example in release mode, signs it ad hoc with
`bundle/arcbox.dev.entitlements` (the hypervisor entitlement needs no
Developer ID), and runs the baseline, the negative controls, two
concurrent clones and its sweeps. They need macOS 15+ on Apple silicon.
`run.sh`'s 1 GiB sweep writes up to 1 GiB into the work directory;
`run-blk.sh` and `run-linux.sh` need APFS for `clonefile`. `run-linux.sh`
boots the newest kernel under `~/.arcbox/boot/` (override with
`SPIKE_KERNEL`) and builds its initramfs from `checkpoint_linux_spike/guest/`
into the work directory, so it needs the musl cross toolchain and nothing
checked in prebuilt.

The guests are checked in prebuilt as `guest.bin`. After editing one,
rebuild it with the musl cross toolchain
(`brew install FiloSottile/musl-cross/musl-cross`):

```bash
# checkpoint_spike (assembly)
cd virt/arcbox-vmm/examples/checkpoint_spike
aarch64-linux-musl-as -o /tmp/guest.o guest.S
aarch64-linux-musl-ld -Ttext=0x40000000 -o /tmp/guest.elf /tmp/guest.o
aarch64-linux-musl-objcopy -O binary /tmp/guest.elf guest.bin

# checkpoint_blk_spike (C)
virt/arcbox-vmm/examples/checkpoint_blk_spike/guest/build.sh
```
