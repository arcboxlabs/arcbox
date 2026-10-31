# HV checkpoint probes

`virt/arcbox-vmm/examples/checkpoint_spike` checkpoints a
Hypervisor.framework VM in one process and resumes it in another. It builds
its own machine; the vCPU, GIC and RAM code lives in `checkpoint_common/`.

| Script | Example | Guest | Write-up |
|---|---|---|---|
| `run.sh` | `checkpoint_spike` | one vCPU, timer only | `docs/experiments/2026-10-31-hv-checkpoint-spike.md` |

```bash
tests/bench/hv-checkpoint/run.sh [work-dir]
```

The script builds its example in release mode, signs it ad hoc with
`bundle/arcbox.dev.entitlements` (the hypervisor entitlement needs no
Developer ID), and runs the baseline, the negative controls, two
concurrent clones and its sweeps. It needs macOS 15+ on Apple silicon.
The 1 GiB sweep writes up to 1 GiB into the work directory.

The guest is checked in prebuilt as `guest.bin`. After editing it,
rebuild it with the musl cross toolchain
(`brew install FiloSottile/musl-cross/musl-cross`):

```bash
cd virt/arcbox-vmm/examples/checkpoint_spike
aarch64-linux-musl-as -o /tmp/guest.o guest.S
aarch64-linux-musl-ld -Ttext=0x40000000 -o /tmp/guest.elf /tmp/guest.o
aarch64-linux-musl-objcopy -O binary /tmp/guest.elf guest.bin
```
