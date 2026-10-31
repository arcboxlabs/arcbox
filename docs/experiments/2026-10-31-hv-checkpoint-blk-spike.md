# 2026-10-31 — Can a multi-vCPU HV VM with a virtio-blk device under load be checkpointed and resumed?

- Type: experiment
- Area: `virt/arcbox-vmm` (blk worker, device manager), `virt/arcbox-hv` (HV FFI); input to the host-side sandbox checkpoint design
- Outcome: open (spike). One production change: the blk worker no longer keeps its avail cursor on the host
- Probes: `virt/arcbox-vmm/examples/checkpoint_blk_spike/`, driven by `tests/bench/hv-checkpoint/run-blk.sh`
- Follows: [the single-vCPU spike](2026-10-31-hv-checkpoint-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9, 15 cores), 48 GB, macOS 26.5
  (25F71). Release build. Guest: a freestanding C program (`guest/`, 6 KiB)
  at EL1 with the MMU off on 1–12 vCPUs, in-kernel GICv3, 64 MiB RAM, and
  one ArcBox `VirtioBlock` over a 16 MiB file on APFS.

## Question

The first spike resumed one vCPU with no devices. A sandbox VM has several
vCPUs and virtio devices whose state is split between the VMM (transport
registers, worker threads, the disk file) and guest RAM (the rings). The
questions were what multi-vCPU restore needs beyond the first spike's
order, which device state has to be saved, how to stop a device that has
requests in flight, and whether disk and memory come back consistent.

## Hypotheses

1. The first spike's order (system registers, GIC blob, ICC, the rest)
   extends to several vCPUs if every vCPU has its MPIDR before
   `hv_gic_set_state`.
2. A virtio-blk device's state is its `VirtioMmioState` plus guest RAM;
   nothing else on the host needs saving.
3. Stopping the vCPUs, then joining the blk workers, leaves no request in
   flight, so no in-flight request needs to be saved.
4. An APFS clone of the disk taken at that point, together with the RAM
   image, gives the guest a disk consistent with what it remembers.

## Method

Every vCPU of the guest takes 1 kHz virtual-timer interrupts (PPI 27) and
drives its own queue of one multi-queue virtio-blk device. It writes a
4 KiB block stamped with a sequence number, issues a FLUSH after every
fourth write, and reads back another of its blocks, checking it against
the sequence number it recorded in RAM for that block. CPU 0 negotiates
the device (`VERSION_1`, `EVENT_IDX`, `MQ`), sets up every queue, routes
the device's SPI to itself and acknowledges it. A failed check stores to
`FAIL_MMIO`, which ends the run with an error.

The host side is the production device path, without the boot code:
`DeviceManager` decodes the guest's MMIO exits, `blk_io_worker_loop`
threads (one per queue) do the I/O, and completions raise the SPI through
`IrqChip` into the in-kernel GIC, all wired the way `darwin_hv/setup.rs` and
`lifecycle.rs` wire them.

`save` pauses in this order: set a stop flag and `hv_vcpus_exit` every
vCPU, repeating the exit until every vCPU has reported. Each vCPU captures
its registers on its own thread and stays parked, alive. Then drop the blk
doorbells and join the workers, which finishes every chain a worker had
popped. Then read the GIC blob, `fsync` the disk and `clonefile` it, write
`mem.bin`, the per-vCPU state and `mmio.txt` (`VirtioMmioState`), and
release the vCPU threads. The guest counter is stored relative to one host
time for all vCPUs.

`restore` runs in a fresh process: clone `disk.img` into a scratch file,
map `mem.bin` `MAP_PRIVATE`, build the machine, write `mmio.txt` into the
new device's `VirtioMmioState`, and replay the side effects the guest's
status writes had on the device object (`ack_features`, `activate`).
Then spawn the workers and create every vCPU. Every vCPU writes its system
registers; after a barrier, vCPU 0 writes the GIC blob, re-asserts the
device's SPI level from `interrupt_status`, and takes one host time for
all counters; after a second barrier, every vCPU writes its ICC registers,
GPRs, SIMD and timer and runs. It reports the time to a tick and to a
completed I/O on every vCPU, and the per-vCPU tick and I/O rates over the
next 100–500 ms.

`run-blk.sh` runs the baseline, the negative controls (`SPIKE_SKIP`,
`SPIKE_LATE_GIC`), two concurrent clones, 20 save/restore cycles and a
1/2/8/12-vCPU sweep. The before/after for the blk worker change was run by
hand: the probe built against the worker at `HEAD`, three saves and
restores.

## Results

Baseline, 4 vCPUs:

| Run | First tick, all vCPUs | First I/O, all vCPUs | Tick rate per vCPU | Peak RSS |
|---|---|---|---|---|
| restore, `MAP_PRIVATE` | 4.33 ms | 4.38 ms | 1.00 kHz each | 11 MiB |
| restore, eager copy | 9.65 ms | 9.72 ms | 1.00 kHz each | 75 MiB |
| clone A (concurrent) | 7.29 ms | 7.28 ms | 0.99–1.00 kHz | 11 MiB |
| clone B (concurrent) | 14.99 ms | 14.99 ms | 1.00 kHz each | 11 MiB |

`mem.bin` and `disk.img` had the same SHA-256 before and after both clones
ran. Save: vCPU pause and capture 0.03–0.07 ms (one outlier at 25.7 ms in
another run), device drain 0.04–1.1 ms, disk `fsync` + clone 4–17 ms,
writing 64 MiB of RAM 24–30 ms. GIC blob 126,362 bytes.

Twenty save/restore cycles, 4 vCPUs: 20 of 20 restores verified. A request
was in flight on 45 of the 80 queues when the pause took hold, and the drain
completed every one; no queue had a request in its avail ring that the
device had not seen. First I/O on every vCPU after restore: min 3.28 ms,
p50 3.62 ms, p90 6.37 ms, max 9.99 ms.

vCPU sweep: 1, 2, 8 and 12 vCPUs all restored with every vCPU ticking at
1.00 kHz and completing I/O; first I/O on every vCPU 3.1–7.9 ms.

Negative controls, same checkpoint:

| Left out | Result |
|---|---|
| `VirtioMmioState` | I/O stops on every vCPU; the SPI fires continuously (20,793 → 547,423 in 2 s) |
| disk clone (blank disk) | the guest's first read-back check fails |
| GIC blob | no timer tick within 2 s; in one run the process died with SIGSEGV |
| ICC registers | no timer tick within 2 s; SPIs not delivered either; I/O continues by polling |
| GIC blob read after the vCPU threads exited (`SPIKE_LATE_GIC`) | no timer tick within 2 s on any vCPU; SPIs and I/O continue |

Blk worker avail cursor kept on the host (the worker at `HEAD`): 3 of 3
restores failed the guest's read-back check within the first second.

## Findings

1. **Read the GIC blob while every vCPU still exists.** A vCPU thread that
   returns destroys its vCPU. A blob read afterwards restores without
   error, SPIs work, but no vCPU ever receives its timer PPI again: the
   destroyed vCPUs' redistributors are gone from it. An early version of
   the probe did this and failed in two ways that looked unrelated: the
   timer never fired again (1, 2 and 4 vCPUs), and when a vCPU had been
   inside its interrupt handler at the checkpoint, its EOI after the
   restore made the framework kill the process (`FIXME: "Handle no active
   virtual interrupt, EOIR1 0x20"`, SIGTRAP). With the blob read while the vCPUs are parked, 15 of 15
   checkpoints taken without regard to interrupt state restored. In 4 of
   them a vCPU was inside its interrupt handler (`ICC_AP1R0_EL1 =
   0x100000`) and finished it after the restore. The pause therefore needs
   no "interrupt boundary".
2. **Hypothesis 1 holds, with one correction to the first spike.**
   Redistributor frames are assigned in vCPU *creation* order, not by
   MPIDR: vCPU index 3 created second got the second frame. MPIDR must
   still be written before `hv_gic_set_state` (the first spike's
   `sysfirst` control), and a restore must create its vCPUs in the order
   the checkpoint did. The probe creates them concurrently and the order
   matched by luck. A real implementation creates them sequentially and
   records the order.
3. **No vCPU may run until all exist.** From the seventh vCPU on,
   `hv_vcpu_create` blocked indefinitely when another vCPU of the VM was
   already running, and a later `hv_vcpus_exit` blocked behind it. All
   vCPU threads now wait on a barrier before their first run.
4. **One `hv_vcpus_exit` does not always stop every vCPU.** With a single
   request per pause, 3 of 30 saves never stopped one vCPU (the main thread
   waited on it forever); the guest exits to the VMM for MMIO tens of
   thousands of times a second, so the request regularly lands while a
   vCPU is outside `hv_vcpu_run`. Repeating the request every millisecond
   until every vCPU has reported stopped all vCPUs in every save of this
   spike (over 60).
5. **Hypothesis 2 was false at `HEAD`, true after one change.**
   `blk_io_worker_loop` kept its avail cursor in a host-side local, so a
   restored worker started at 0 and replayed stale descriptors. The guest
   read back the wrong data in 3 of 3 runs. Every chain a drain pops is completed
   before the drain returns, so between drains the cursor equals the
   device's `used.idx`, which is in guest RAM. The worker now derives
   it there (`drain_queue`). With that change the device's state is
   `VirtioMmioState` plus guest RAM, plus two side effects to replay on the
   device object: acknowledged features and activation. `EVENT_IDX`'s
   `avail_event` is in guest RAM too.
6. **Hypothesis 3 holds for this device.** Stopping the vCPUs first and
   joining the workers second leaves no popped chain unfinished. In-flight
   requests were the common case (45 of 80 queues, mostly a FLUSH in
   `fsync`). No request ever sat in an avail ring without its kick. A kick
   the device received is always drained, so restore does not need to
   ring the queues. A future device that keeps requests in flight across
   drains (an async I/O backend) breaks this and must be quiesced
   differently.
7. **Hypothesis 4 holds.** `fsync` then `clonefile` after the drain gives a
   disk the guest's RAM agrees with on every read-back (thousands per run).
   The blank-disk control fails on its first read. `clonefile` took
   4–17 ms for a 16 MiB file with a few MiB written; per-run restore clones
   are what keep the checkpoint itself unchanged.
8. **An inconsistent restore can take the VMM process down.** Skipping the
   GIC blob ended in SIGSEGV inside the framework, and the late-blob probe
   above ended in SIGTRAP. A restore is only safe in a process of its own,
   which HV's one-VM-per-process rule already forces.
9. Host noise, not checkpoint state: with 8 or more busy vCPUs (plus the
   blk workers' spinning FLUSH barrier) single vCPUs lag behind 1 kHz by up
   to 25 % over a 300 ms window, before a checkpoint as much as after. The
   probe checks the mean rate and that every vCPU ticks.

## Decisions taken / open

Taken: the blk worker derives its avail cursor from `used.idx`
(`virt/arcbox-vmm/src/blk_worker.rs`). Its unit tests pass. A daemon-level
e2e run of the change is still owed before it ships.

Open for the host-side sandbox checkpoint design:

- The net, vsock and console devices: each has host-side state (TX
  cursors in `VirtioNet`, vsock connections) that this spike did not
  touch.
- Recording vCPU creation order in the checkpoint and creating vCPUs
  sequentially on restore.
- A Linux guest: boot it, checkpoint mid-I/O, resume it. The bare-metal
  guest exercises the mechanisms, not Linux's use of them (PSCI CPU_OFF,
  WFI-parked vCPUs, the page cache).
- Saving only touched pages (first spike, finding 6).
