# 2026-10-31 — Can the production HV VMM checkpoint a Linux guest in the middle of block I/O and resume it?

- Type: experiment
- Area: `virt/arcbox-vmm` (`darwin_hv`: vCPU loop, lifecycle, setup, new `checkpoint`), `virt/arcbox-hv` (`snapshot`); input to the host-side sandbox checkpoint design
- Outcome: open (spike). The production HV machine gains a checkpoint and restore path (`Vmm::hv_checkpoint`, `Vmm::hv_restore`) and now creates its vCPUs in id order before any of them runs
- Probes: `virt/arcbox-vmm/examples/checkpoint_linux_spike/`, driven by `tests/bench/hv-checkpoint/run-linux.sh`
- Follows: [the multi-vCPU virtio-blk spike](2026-10-31-hv-checkpoint-blk-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5 (25F71). Release
  build. Guest: the ArcBox kernel from boot assets 0.8.12 (Linux
  6.18.38-arcbox) on 1–8 vCPUs with 1 GiB RAM, booting a 58 KiB initramfs,
  with one writable 256 MiB disk on APFS, virtio-console and virtio-rng;
  no network, vsock, VirtioFS or balloon.

## Question

The first two spikes resumed bare-metal guests on a probe-built machine.
This one asks whether the same mechanisms hold for Linux on the machine
the daemon actually runs: Linux's own use of the GIC, timers, PSCI and
WFI; a vCPU that is powered off at the checkpoint; I/O on both block
paths the guest kernel has, the virtio-blk queues and the synchronous HVC
fast path (`arcbox_hvc_blk`); and how much of `HvMachine` has to change.

## Hypotheses

1. Stopping each vCPU at the top of its run loop, where the previous exit
   has been handled completely, is a valid checkpoint point for Linux.
   No instruction boundary or interrupt boundary is needed.
2. The host-side state of this machine is: per-vCPU registers, the GIC
   blob, each virtio device's `VirtioMmioState`, the PSCI power states and
   the PL031's four registers, plus guest RAM and the disk. The HVC fast
   path keeps no state; it is quiesced once its vCPU stops.
3. A vCPU that was off at the checkpoint can be restored as "created, off"
   and onlined later by the guest with PSCI CPU_ON.
4. Restore fits the production start path: the same `initialize` and
   `start`, with RAM mapped from the checkpoint and the boot image skipped.

## Method

`HvMachine::checkpoint` (`darwin_hv/checkpoint.rs`):

1. Set a checkpoint flag and `hv_vcpus_exit` every vCPU every millisecond,
   until every vCPU the PSCI registry reports on has captured its state.
   A vCPU sees the flag at the top of its run loop (`vcpu_loop.rs`),
   captures `VcpuState` on its own thread and parks with its `HvVcpu`
   alive. A vCPU that is off waits for CPU_ON outside `hv_vcpu_run`
   anyway.
2. Take one host time as the pause instant for every vCPU's counter.
3. Drop the blk doorbells and join the workers.
4. Read the GIC blob, every device's transport state, the PSCI power
   states and the PL031. `fsync` and `clonefile` each disk. Write the
   non-zero 16 KiB pages of RAM to a sparse `mem.bin`.

The caller then stops the machine.

`Vmm::hv_restore` loads `state.json`, then runs the normal `start`:

- `initialize` maps `mem.bin` `MAP_PRIVATE` as guest memory and skips the
  kernel, initrd and FDT (`load_boot_image`, split out of
  `initialize_darwin_hv`).
- `start` writes the device state back, replaying `ack_features` and
  `activate`, and sets the saved power states and PL031. It then spawns the
  vCPU threads with `VcpuBoot::Restore`.
- Each thread creates its vCPU in id order and writes its system
  registers. After a barrier, one thread writes the GIC blob, re-asserts
  every device's line from `interrupt_status` and takes one host time.
  After a second barrier, each thread writes its ICC registers, GPRs, SIMD
  registers and timer.
- A vCPU that was on continues from those registers. One that was off gets
  vCPU 0's counter offset and waits for CPU_ON.
- `hv_restore` returns once every vCPU has passed these phases, or with the
  first vCPU's error.

The guest's `/init` (`guest/init.c`) runs four I/O threads with `O_DIRECT`
on 4 KiB blocks of the one disk, each thread in its own 16 MiB region:

- Threads 0 and 1 use `/dev/vda` (virtio-blk). Threads 2 and 3 use
  `/dev/arcboxhvc0` (the HVC path to the same image).
- Each thread writes random blocks stamped with (thread, block, seq) and
  reads back another block, checking it against the seq it keeps in RAM.
  It calls `fdatasync` every 256 writes.
- A ticker prints `tick n mono_ms … ops a b c d fails f` on hvc0 once a
  second.

A restore passes when all of these hold:
- The first tick after the restore is the saved tick plus one.
- Exactly one tick period (900–1500 ms) of `CLOCK_MONOTONIC` separates the
  saved tick from the first tick after the restore.
- Every thread completes I/O in every tick interval from the saved tick on.
- No read-back check fails.

`run-linux.sh` runs the following:
- A baseline.
- The negative controls listed below.
- Two concurrent restores of one checkpoint.
- 20 save/restore cycles, each paused at a pid-derived offset inside a tick
  period.
- A 1/2/8-vCPU sweep.
- A checkpoint taken with `maxcpus=2`, after whose restore the guest
  onlines CPUs 2–3 at tick 7 (`spike_online_at=7`).

## Results

Baseline, 4 vCPUs: Linux reaches `/init` and its first tick 1.2 s after
`start`. The checkpoint took 328 ms in total:

| Step | Time |
|---|---|
| Stopping all vCPUs | 1.5 ms |
| Draining the I/O workers | 0.1 ms |
| Disk `fsync` + clone | 25 ms |
| Scanning 1 GiB of RAM and writing 3,473 non-zero pages (54 MiB) | 301 ms |

`hv_restore` returned in 2.8 ms. The guest's next tick was 7, with
`CLOCK_MONOTONIC` 1,002 ms after the saved tick 6. Over the next 5 s the
four threads completed 13,916, 13,401, 21,072 and 22,537 operations, with
0 failures.

20 save/restore cycles at random points: 20 of 20 passed. `hv_restore`
min 2.6 ms, p50 2.8 ms, max 4.2 ms. Stopping the vCPUs took min 1.3 ms,
p50 1.5 ms, max 6.1 ms. Two concurrent restores of one checkpoint both
passed, and `mem.bin` and `disk0.img` had the same SHA-256 afterwards.

vCPU sweep: 1, 2 and 8 vCPUs passed. With `maxcpus=2` the checkpoint
recorded `cpu_on = [true, true, false, false]`. After the restore the guest
onlined CPUs 2–3 (`online 4`), and all four threads kept passing.

Negative controls, each on a copy of the baseline checkpoint:

| Changed | Result |
|---|---|
| device state dropped (`devices: []`) | no tick within 5 s (console and blk queues not ready) |
| every vCPU's timer offset set to 0 | no tick within 5 s |
| every vCPU's ICC registers zeroed | no tick within 5 s |
| empty GIC blob | `hv_gic_set_state` returns "bad argument"; `hv_restore` fails |
| GIC blob of a `maxcpus=2` checkpoint (redistributors 2–3 never enabled) | fails in each of 3 runs, differently: thread 2 stops for good; no tick within 5 s; SIGTRAP |
| GIC blob of another all-on checkpoint (same configuration) | 0 or 1 byte differs; identical: passes; one byte: SIGTRAP, `FIXME: "Handle no active virtual interrupt, EOIR1 0x1"` |
| RAM of another checkpoint | guest oops (`FPAC`) or no tick within 5 s |
| disk replaced by an empty one (`SPIKE_TAMPER=disk`) | read-back checks fail within the first tick (14,021 in 5 s) |

A fresh boot that is never checkpointed shows the same stall as runs after
a restore. About every 8 s, a one-second tick interval completes only
about 100 operations. The stall is a
property of the workload on this host, not of restores, and the per-tick
check tolerates it because the counts still grow.

## Findings

1. **Hypotheses 1–4 hold for Linux on the production machine.** The pause
   point needs no instruction or interrupt boundary. All 20 pauses at
   random points resumed. The probe does not record where each vCPU was;
   an HVC block call in progress completes before its vCPU reaches the
   pause point.
2. **The HVC fast path needs nothing.** It is synchronous on the vCPU
   thread, so a parked vCPU has no request in flight. Its two threads did
   more I/O than the virtio-blk ones and verified as well after every
   restore.
3. **The GIC state is mostly configuration, but its active bits are
   per-checkpoint and fatal when wrong.** Two checkpoints of the same
   machine produced byte-identical 126 KB blobs, or blobs one byte apart.
   With that byte swapped, a restored vCPU wrote an EOI for INTID 1 (an
   SGI) that the swapped blob did not have active, and the framework
   killed the process, the same SIGTRAP spike 2 hit with a late GIC read.
   The GIC blob and ICC registers must come from the same instant.
4. **Restoring off vCPUs works without capturing them.** Their threads
   were outside `hv_vcpu_run`, so the GIC read included their
   redistributors. On restore they only need to exist in the right
   creation order and share vCPU 0's counter offset. Linux's later CPU_ON
   brought them up normally.
5. **The production machine needed one ordering fix to be restorable.** It
   created vCPUs concurrently, so redistributor frames (assigned in
   creation order, spike 2) depended on thread scheduling. The BSP could
   also run before the secondaries existed, which spike 2 showed can block
   `hv_vcpu_create` from the seventh vCPU on. `CreationOrder`
   (`vcpu_loop.rs`) now creates vCPUs in id order and holds every thread
   until all exist, for fresh boots too. Frame `i` is vCPU `i` in every
   machine, so the checkpoint does not need to record the order.
6. **Restore errors happen on vCPU threads after `start` returns.**
   `hv_restore` has to wait for every vCPU to pass its restore phases to
   report them. An empty GIC blob first showed this: `start` succeeded,
   every vCPU logged the error, and the guest never ran.
7. **Guest wall time freezes across a checkpoint.** `CLOCK_REALTIME`
   resumes from where it stopped, because the guest reads the PL031 only
   at boot. A resumed sandbox needs the agent's clock push (the ABX-416
   ping path) after every restore, not only after boot.
8. **The checkpoint cost is RAM, not pause.** The vCPUs stop in 1.5 ms. A
   1 GiB guest takes 300 ms to scan for non-zero pages; the guest had
   touched 54 MiB. Saving only pages the guest has written (first spike,
   finding 6) is the next step for checkpoint latency. Restore is
   independent of RAM size because `mem.bin` is mapped lazily.

## Decisions taken / open

Taken:

- vCPUs are created in id order, and no vCPU runs until all exist
  (`vcpu_loop.rs`).
- `HvMachine` has a checkpoint path, behind the `gic` feature.
- `VirtioRng` derives its avail cursor from `used.idx`, as the blk worker
  does since spike 2 (`virt/arcbox-virtio-rng/src/lib.rs`). A restored rng
  device would otherwise replay stale descriptors.

These changes need the daemon-level HV e2e before they ship.

Open:

- The net, vsock and VirtioFS devices, and the balloon, are still outside
  the checkpoint: the spike guest has none of them. Each keeps host-side
  state (TX cursors, vsock connections, FUSE inode tables, DAX mappings).
- The console's RX cursor (`rx_last_avail`) is host-side. This spike only
  writes to the console, so the RX cursor was never exercised.
- Dirty-page tracking, so a checkpoint writes only what changed.
- A guest clock resync after restore, via the agent.
