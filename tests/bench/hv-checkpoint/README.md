# HV checkpoint probes

Two examples in `virt/arcbox-vmm/examples/` checkpoint a
Hypervisor.framework VM in one process and resume it in another. Both
build their own machine and share the vCPU, GIC and RAM code in
`checkpoint_common/`.

| Script | Example | Guest | Write-up |
|---|---|---|---|
| `run.sh` | `checkpoint_spike` | one vCPU, timer only | `docs/experiments/2026-10-31-hv-checkpoint-spike.md` |
| `run-blk.sh` | `checkpoint_blk_spike` | 1–12 vCPUs, timer, ArcBox virtio-blk under load | `docs/experiments/2026-10-31-hv-checkpoint-blk-spike.md` |

```bash
tests/bench/hv-checkpoint/run.sh [work-dir]
tests/bench/hv-checkpoint/run-blk.sh [work-dir]
```

Each script builds its example in release mode, signs it ad hoc with
`bundle/arcbox.dev.entitlements` (the hypervisor entitlement needs no
Developer ID), and runs the baseline, the negative controls, two
concurrent clones and its sweeps. They need macOS 15+ on Apple silicon.
`run.sh`'s 1 GiB sweep writes up to 1 GiB into the work directory;
`run-blk.sh` needs APFS for `clonefile`.

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
