# 2026-10-31 — Can a Hypervisor.framework VM be checkpointed in one process and resumed in another?

- Type: experiment
- Area: `virt/arcbox-hv` (HV FFI); input to the host-side sandbox checkpoint design
- Outcome: open (spike; no production code path uses it yet)
- Probes: `virt/arcbox-vmm/examples/checkpoint_spike/` (shared state code in `checkpoint_common/`), driven by `tests/bench/hv-checkpoint/run.sh`
- Followed by: [the multi-vCPU virtio-blk spike](2026-10-31-hv-checkpoint-blk-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5 (25F71), load
  average 2–3 during the runs. Release build. Guest: one vCPU, bare-metal
  EL1 program (`guest.S`, 4 KiB) with the MMU off, in-kernel GICv3
  (`hv_gic_*`), virtual timer at 1 kHz as PPI 27; 64 MiB or 1 GiB of guest
  RAM.

## Question

Hypervisor.framework allows one VM per process, so a host-side sandbox on HV
needs one VMM process per VM, and a checkpoint has to be resumed in a
different process from the one that took it. The open questions were
whether Apple's in-kernel GIC can be saved and restored at all from
outside the framework, what else besides the GIC has to come back, and
what a restore costs. The answers decide whether HV checkpoint is a
tractable piece of the sandbox work and how a restore maps guest memory.

## Hypotheses

1. Registers read with `hv_vcpu_get_*`, the opaque blob from
   `hv_gic_state_get_data`, and guest RAM are enough to resume the guest in
   a new process.
2. The GIC blob alone carries the interrupt controller; the per-vCPU ICC
   registers do not matter.
3. A `MAP_PRIVATE` mapping of the memory file can back guest RAM through
   `hv_vm_map`, so restore cost does not grow with the checkpoint's size.
4. The guest's virtual counter has to be carried across the checkpoint;
   the new vCPU's default offset is not good enough.

## Method

The guest arms the virtual timer at 1 kHz (drift-free, `CNTV_CVAL += period`),
counts interrupts and WFI wakeups in a shared data page, and on every
wakeup checks invariants held in registers: `x20 == 3 * x19`, constants in
`x21`/`x22`, and a 128-bit constant in SIMD register `v5`. A broken
invariant stores to an unmapped address, which exits to the host as an
error.

`checkpoint_spike save` boots the guest, lets it run, stops the vCPU with
`hv_vcpus_exit`, and on the vCPU thread reads X0–X30, PC, CPSR, FPCR, FPSR,
Q0–Q31, 40 EL1/EL0 system registers (`SYS_REGS` in the probe), the 9
writable ICC registers, the vtimer mask, and the guest counter
(`mach_absolute_time() - vtimer_offset`). It then takes the GIC blob and
writes guest RAM sparsely (non-zero 16 KiB pages only).

`checkpoint_spike restore` runs in a fresh process: map `mem.bin`, create
the VM, GIC and vCPU, write the state back, set the vtimer offset so the
guest counter resumes where it stopped, and run. It reports the time from
entering `restore` to the vCPU running, to the first new timer tick and to
the first wakeup, the guest-time gap between the last tick before the
checkpoint and the first after it, the tick rate over the next 200–500 ms,
and the process's peak RSS. Process exec and dyld time are not included.

`SPIKE_SKIP` leaves one part out as a negative control. `SPIKE_RAM_MB` and
`SPIKE_FILL_MB` size guest RAM and pre-dirty part of it to stand in for a
guest's touched memory. `run.sh` runs the whole matrix below once, then
20 restores for the latency distribution.

## Results

Baseline, 64 MiB RAM (3 non-zero pages):

| Run | Ticks before → after | Guest time gap | Tick rate | First tick | Peak RSS |
|---|---|---|---|---|---|
| restore, `MAP_PRIVATE` | 309 → 819 | 0.73 ms | 0.998 kHz | 2.19 ms | 8 MiB |
| restore, eager copy | 309 → 819 | 0.73 ms | 0.998 kHz | 9.47 ms | 72 MiB |
| clone A (concurrent) | 309 → 819 | 0.73 ms | 0.998 kHz | 2.37 ms | 8 MiB |
| clone B (concurrent) | 309 → 819 | 0.74 ms | 0.998 kHz | 2.32 ms | 8 MiB |

`mem.bin` had the same SHA-256 before and after both clones ran.

Negative controls, same checkpoint:

| Left out | Result |
|---|---|
| GIC blob | no tick within 1 s; ~1 M WFI wakeups (interrupt pending, never taken) |
| ICC registers | no tick within 1 s; ~1.2 M WFI wakeups |
| system registers written after the GIC blob (`sysfirst`) | no tick within 1 s; ~1 M WFI wakeups |
| SIMD registers | guest reported a broken invariant at its first wakeup |
| vtimer offset (2 s between save and restore) | guest time jumped 2,270 ms; 2,269 timer interrupts fired back to back (ticks 208 → 2,687, wakeups 208 → 418) |

With the offset restored after the same 2 s gap, the gap was 0.99 ms.

Size sweep, 1 GiB guest RAM:

| Dirty | `mem.bin` allocated | Save: memory write | Restore lazy: first tick / RSS | Restore eager: first tick / RSS |
|---|---|---|---|---|
| 0 MiB | 2 MiB | 297 ms | 2.11 ms / 8 MiB | 82.0 ms / 1,032 MiB |
| 64 MiB | 68 MiB | 321 ms | 2.31 ms / 8 MiB | 81.7 ms / 1,032 MiB |
| 256 MiB | 260 MiB | 386 ms | 2.50 ms / 8 MiB | 79.2 ms / 1,032 MiB |
| 1,000 MiB | 1,004 MiB | 291 ms | 3.73 ms / 8 MiB | 71.4 ms / 1,032 MiB |

vCPU and GIC capture took 2.2–2.3 ms in every save; the GIC blob was
126,336 bytes. Twenty lazy restores (1 GiB RAM, 256 MiB dirty), entry to
first tick: min 2.50 ms, p50 2.61 ms, p90 2.75 ms, max 2.79 ms.

## Findings

1. **Hypothesis 1 holds for this guest.** Registers, SIMD, system
   registers, ICC registers, the GIC blob, the vtimer offset and mask, and
   RAM resume a vCPU in a new process. Ticks continue at 1 kHz and every
   register invariant survives. One vCPU and no devices only: multi-vCPU
   ordering and device state are untested.
2. **Hypothesis 2 is false.** Without the ICC registers no interrupt is
   delivered. The SDK header says so (`hv_gic_state_get_data`: "complete
   serialized state of the device, except for the GIC cpu registers"); the
   control confirms it.
3. **Restore order is load-bearing: `MPIDR_EL1` before `hv_gic_set_state`.**
   `hv_gic_get_redistributor_base` returns `HV_BAD_ARGUMENT` until MPIDR is
   written, and with the blob restored first no interrupt arrives. The
   probe writes all system registers, then the blob, then ICC. (The
   follow-up spike found that redistributor frames are assigned in vCPU
   creation order, not by MPIDR.)
4. **Hypothesis 3 holds.** `hv_vm_map` accepts a `MAP_PRIVATE` file
   mapping. Restore to first tick is 2–4 ms whatever the checkpoint's size,
   peak RSS stays at 8 MiB, and concurrent clones leave the file
   untouched. The eager copy costs ~75–80 ms per GiB of guest RAM and the
   full RAM in RSS. Not measured: the cost of a guest faulting in a large
   working set through the file mapping after resume.
5. **Hypothesis 4 holds.** The new VM's default offset makes guest time
   jump by the time the checkpoint sat on disk, and the timer fires every
   missed period at once. Carrying `cntvct` across keeps the gap at one
   tick.
6. **Saving scans all of RAM.** With 3 dirty pages, saving 1 GiB still
   took ~300 ms, which is the zero-page scan. A real implementation should
   write only pages the guest touched, not test every page.
7. Unrelated defect fixed along the way: `arcbox-hv`'s `HV_SYS_REG_TTBR1_EL1`,
   `HV_SYS_REG_SPSR_EL1` and `HV_SYS_REG_ELR_EL1` had wrong encodings (the
   latter two named the EL2 registers). Nothing used them.
8. Apple's GIC leaves `GICR_WAKER` to the VMM: reads and writes trap as
   data aborts. The probe treats them as read-as-zero, write-ignored, as
   the HV VMM's MMIO bus does.

## Decisions taken / open

None taken. Open for the host-side sandbox checkpoint design (multi-vCPU
and device state are answered by the
[follow-up](2026-10-31-hv-checkpoint-blk-spike.md)):

- First-touch cost of a Linux guest's working set through the file
  mapping.
- Whether the GIC blob survives a macOS update ("versioned for detecting
  incompatibilities" per the header); warm checkpoints may need the OS
  build in their key.
