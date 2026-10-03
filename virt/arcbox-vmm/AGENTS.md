# arcbox-vmm Agent Guidance

Scope: the macOS Hypervisor.framework (HV) backend. VZ is the oracle — HV-only
red points at code here; double red points above the hypervisor. The parent
`virt/AGENTS.md` owns the SplitQueue and validation-ladder invariants; this
file owns the HV-framework-specific footguns.

## Async-Worker Completion Contract (read first)

Any async worker that completes guest I/O (blk, net-rx, vsock, console, or a
future device) does exactly two things, in this order:

1. `VirtioMmioState::trigger_interrupt(1)` — set interrupt_status (INT_VRING).
2. Fire the `DeviceIrqCallback` (`irq_callback(irq, true)`) — assert the SPI.

Asserting the SPI is the whole wake. With the in-kernel GIC (`hv_gic`, the
only configuration this VMM boots with) the framework handles WFI inside
`hv_vcpu_run`: an idle vCPU's thread blocks in
`HvCore::Hypervisor::VcpuStateManager::wait_for_interrupt`, `hv_gic_set_spi`
signals that wait, and a vCPU running guest code takes the SPI
asynchronously. Measured 2026-09-30 on macOS 26.4: every vCPU's `wfi` and
`vtimer` exit counters stay 0 over a full boot, and guest cross-CPU wakeups
(FIFO ping-pong between cpuset-pinned containers) take 30–55 µs.

**Do not add an `hv_vcpus_exit` kick after the SPI.** Every worker used to,
on the belief that a WFI-idle vCPU sat parked on the host; the kick was a
forced `Canceled` exit for nothing. Removing all of them (c3004580): vsock
RPC p50 0.81 → 0.30 ms (0.96 → 0.32 ms with the target CPU busy), 1 GiB
stdin pipe 5.9–6.4 → 5.4–5.9 s, `docker load` 300 MB 3.2–4.4 → 2.7–3.3 s,
idle daemon CPU 9.3 → 6.7%. The same goes for unparking vCPU threads from
the IRQ callback: nothing is parked there. `hv_vcpus_exit` remains the right
tool for `stop`/`pause`, where the point is to leave `hv_vcpu_run`.

Reference: `blk_worker.rs::trigger_irq`; the GIC callback in `setup.rs`
(step 5) is `set_spi` and nothing else. The vCPU loop's WFI branch
(`vcpu_loop.rs`) is unreachable in this configuration and kept only for a
framework that does trap WFI — its 1 ms `park_timeout` bounds latency there,
since no SPI unparks it.

## vCPU Exit Loop: PC-advance is asymmetric by exit class

Hypervisor.framework auto-advances ELR/PC on HVC/SMC but NOT on DataAbort
(MMIO) or trapped SystemRegister exits (`vcpu_loop.rs`):

- MMIO (DataAbort): manually `PC += 4` after handling — else the instruction
  re-traps (vcpu_loop.rs:331-334).
- HVC/SMC: do NOT manually advance — auto-advanced; a manual +4 skips an
  instruction (vcpu_loop.rs:378-380).
- Unknown sysreg: treat as RAZ/WI (read → write 0 into Xrt, writes dropped) and
  force `PC += 4`. Without it, Linux early boot writes OSDLR_EL1 and wedges in an
  infinite MSR-trap loop (vcpu_loop.rs:436-469).

Adding a new exit-class handler: decide its advance behavior explicitly; don't
copy a neighbor blindly.

## macOS HV Block Worker Contract

`blk_worker.rs` re-implements only virtio-blk request PARSING/EXECUTION; it does
not inherit that logic from `arcbox-virtio-blk::VirtioBlock`. Feature-bit and
config-space advertisement is NOT duplicated — the HV MMIO device wraps
`VirtioBlock`'s `VirtioDevice` impl, so `register_virtio_device` reads `features()`
(mod.rs:758) and config-space reads hit `read_config` (dispatch.rs:48) through it;
edit those in one place, not two. `arcbox-virtio-blk/AGENTS.md` is the source of
truth for feature semantics and request-parsing validation; keep this worker in
lockstep with it. If the worker cannot honor a feature on P0 macOS, do not
advertise that feature for devices using this backend.

Worker-specific coupling not in the shared doc:

- FLUSH spin-waits on the cross-queue `FlushBarrier.in_flight`
  (blk_worker.rs:345-353, 769-788). Only Read/Write/WriteZeroes enroll (inc/dec)
  in the barrier; DISCARD is deliberately excluded (advisory)
  (blk_worker.rs:332-361). Any NEW data-mutating request type must enroll, or a
  following FLUSH returns before its data hits disk.
- Tests must exercise the worker parser/executor, not just shared leaf helpers.

## HVC block fast path is the System VM's data-disk path

On HV the agent picks `/dev/arcboxhvc1` (HVC hypercalls,
`vmm/darwin_hv/hvc_blk.rs`; driver `drivers/arcbox_hvc_blk.c` in
`arcboxlabs/kernel`) over `/dev/vdb`; `blk_worker.rs` serves only the rootfs
and the ext4 metadata disk. A block feature the data disk needs must exist in
**both** the worker and `hvc_blk.rs` + the driver — DISCARD
(`ARCBOX_HVC_BLK_DISCARD`, 0xC2000005) was missing there while the worker's
hole punch was correct and unused, so no freed block ever left `docker.img`
on HV (`docs/disk-reclaim.md`). The driver probes each hypercall with a
zero-length call at bind time; an unknown function ID must keep falling
through to `handle_psci` and answer `PSCI_NOT_SUPPORTED`, which is what keeps
an old host and a new guest (or the reverse) inert rather than broken.

## macOS HV Net RX Worker Contract

RX injection has two flavors — the channel-based `RxInjectThread`
(`arcbox_net_inject::inject`, preferred) and the legacy kqueue-on-socketpair
`net_rx_worker.rs` (fallback; try_spawn picks between them, net_worker.rs:118) —
and both drive frames through `arcbox_net_inject::queue::inject_one_frame`.
`arcbox-net-inject` is the source of truth for RX ring walk, notify, and
MRG_RXBUF `num_buffers` semantics; a fix there lands for both flavors. Any change
must keep both flavors' `QueueConfig` snapshot and `Arc<GuestMemWriter>` wiring in
lockstep — they share the one inject entry point, so diverging their wiring
silently breaks whichever path is not exercised in your test.

## MMIO Register File

`MAX_VIRTQUEUES` (`device/mmio_state.rs:95`, currently 64) bounds every per-queue
array and every queue-indexed dispatch path. virtio-blk sets
`num_queues = vcpu_count` (one queue per vCPU, `setup.rs:333`), so a bound
smaller than the max supported vCPU count silently drops queue config for
`queue_sel >= bound` and boot-wedges the guest (ABX-386). Keep the regressions in
`device/tests.rs`: `queue_config_beyond_eight_round_trips` (round-trips a
selector > 8) and the near-`u64::MAX` ring-address snapshot test.

## Diagnostic Counters (never reset — post-mortems need history)

- `VirtioMmioState::kicks`/`interrupts` (mmio_state.rs:134,136) and
  `vcpu_stats::VcpuStats` are cumulative across device resets.
- `kick_broadcasts` / `unpark_broadcasts` in the debug snapshot are retired
  (2026-09-30) and always 0: io workers no longer kick and the IRQ callback
  no longer unparks. The fields stay on the wire for the e2e forensics
  mirror. `VcpuStats::kicks_received` still counts `Canceled` exits, which
  now come only from `stop`/`pause`; a non-zero delta during steady state
  means someone put a kick back.
- The R2/R3 acceptance numbers that were read from those counters
  (~2301 unpark-broadcasts / ~71 kick-broadcasts per boot) describe a
  mechanism that no longer exists; the campaign's remaining idle-CPU lever
  is the `rx-inject` thread's yield/poll loop (~5.6 of the ~6.7% idle CPU,
  `sample`d 2026-09-30), not vCPU wakeups.

## Debugging: entry points and failure signatures

Two snapshots (HV only; empty/zero under VZ — devices belong to VZ):

- `Vmm::debug_snapshot` (vmm/mod.rs) — devices + per-vCPU exit counters.
- `DeviceManager::virtio_debug` (device/debug.rs:75) — devices/queues only;
  reads MMIO mirror + live guest ring memory, THROUGH poisoned locks.

Both are observational and MUST be captured while the VM is alive (a stuck boot
is the main use case). Exposed via the `GetVirtioDebug` RPC — served from
`early_runtime`, never gated on readiness (see `app/AGENTS.md`). Console-log
archaeology produced multiple WRONG root causes for ABX-386; snapshot first.

| Symptom | First move | Likely cause |
|---|---|---|
| >8-vCPU cold boot: guest wedges D-state / `folio_wait_bit_common` stall | live snapshot → find a blk queue whose `avail_idx` advances while `used_idx` stays stuck, or config dropped for high `queue_sel` | per-queue register array too small (`MAX_VIRTQUEUES` vs one-queue-per-vCPU), ABX-386 |
| Guest TLS/cert validation fails right after boot | check whether agent-up ping has fired | no RTC; guest sits at kernel default epoch until the post-readiness ping sets the clock (ABX-416) |
| Intermittent guest hang just after an I/O completes | audit the worker's completion path: `trigger_interrupt` then `irq_callback(irq, true)`, and the device must be DRIVER_OK (`sync_irq_level` drops the SPI otherwise) | the SPI was never asserted, or asserted before the guest set DRIVER_OK — see Async-Worker Completion Contract |
| Host→guest vsock bulk (`docker run -i` pipe, `docker load`) slow on HV with the guest idle | diff the vsock device's `interrupts` and RX `used_idx` from `GetVirtioDebug` across one transfer: ~1 interrupt per ≤3776-byte packet means the injection round is back to one packet per connection; a non-zero `kicks_received` delta means a kick came back | `rx_injection.rs` must keep RW pending after a full read (5de23af4); no worker may call `hv_vcpus_exit` (c3004580) |

For config-dependent boot failures, bisect with the `hv_e2e` config-matrix knobs
(`ARCBOX_HV_E2E_VCPUS/MEMORY_MB/BALLOON/BOOT_ONLY/...`, all share the
`ARCBOX_HV_E2E_` prefix) one dimension at a
time — see `virt/AGENTS.md`. This is how ABX-386 was localized to "vCPU count,
threshold exactly 8".

## Teardown ordering is a contract (ABX-415 open SIGSEGV)

`stop_darwin_hv` (lifecycle.rs) has a load-bearing order:

1. Join all vCPU threads (via the targeted `exit_vcpus` + unpark cancel loop).
2. Join every worker that holds a `GuestMemWriter`: blk (457-464), net-rx
   (466-477), vsock (479-486), console (488-495). These reference guest RAM;
   dropping guest memory before they exit is a use-after-free.
3. Cleanup strictly DAX `drain_all` → GIC → `hv_vm` → `hv_guest_mem`
   (497-517). DAX `hv_vm_unmap` must run while the VM is alive; guest memory
   must outlive `hv_vm` so mapped pages stay valid until `hv_vm_destroy`.

ABX-415 (SIGSEGV on SIGTERM) lives in exactly this path — do not reorder without
re-reading every join site's comment.

## vCPU registration ordering (ABX-367)

`hv_vcpus_exit` on arm64 is a silent no-op for NULL/0 — it needs a concrete list
of vCPU IDs. Each vCPU pushes its raw handle then its `Thread` into the shared
registries ONLY after all register-setup calls succeed (vcpu_loop.rs);
pushing earlier risks a dangling handle (UB in Apple's framework) or an
unbounded registry across failed boots. `stop`/`pause` snapshot the registry
when they run and `stop` warns when it is empty while threads are alive
(lifecycle.rs). Only those two paths call `hv_vcpus_exit` now.

## Guest-controlled input

Parent `virt/AGENTS.md` "Guest-controlled input" owns the rule (checked
arithmetic on every guest-programmed value, tests near `u64::MAX`). HV-specific
coverage: the near-`u64::MAX` ring-address snapshot regression in
`device/tests.rs` — keep it green.

## Releasing guest RAM (`vmm/darwin_hv/page_release.rs`)

Guest RAM is an anonymous mapping exposed to the guest through stage-2
(`hv_vm_map`). Once the guest has dirtied a page through that mapping, **no
`madvise` from the host releases it**: `MADV_DONTNEED` and `MADV_FREE_REUSABLE`
both leave `phys_footprint` untouched, with or without an `hv_vm_unmap` first
(measured 2026-09-25, macOS 26.4, with a probe that dirtied the range from a
real vCPU — a host-side `memset` calibration says `MADV_FREE_REUSABLE` works,
and is the wrong experiment). The one sequence that returns memory is
`hv_vm_unmap` → `mmap(MAP_FIXED)` a fresh anonymous mapping over the same host
address → `hv_vm_map` it back: footprint and resident size drop together, the
range reads back zero, and a later guest write is billed honestly (~40 µs per
2 MiB). `Stage2Refresh` is that sequence, installed as the balloon device's
`PageReleaser` in `setup.rs`; the guest's free page reporting drives it with
no host-side target. Two constraints: ranges are aligned *inward* to the
16 KiB host page (XNU rounds a misaligned range outward, `hv_vm_unmap`
refuses sub-page ranges — either would discard the guest's neighbouring 4 KiB
pages), and `hv_vm_unmap`/`hv_vm_map` on a sub-range of a live mapping is
fine only when the *same* host address is mapped back; the DAX window's
no-overlap rule (`setup.rs`) is about mapping a different host range into a
used IPA.

## Platform Gaps

- No RTC: guest wall time comes from the post-readiness agent ping
  (`AgentPingRequest.timestamp_secs` → agent `clock_settime`) until a PL031 device
  lands (ABX-416). Anything time-sensitive before agent-up sees the kernel
  default epoch — TLS cert validation in particular fails.

## The Linux arm is a CI gate now — keep macOS-only code gated

This crate is the platform seam `engine/arcbox-engine` reaches macOS
through, so the engine and computer layers compile on Linux only if this
one does. All three are in CI's `linux-engine` job: the GNU step checks
`arcbox-vmm`, `arcbox-engine`, `arcbox-computer`; the musl step checks
`arcbox-vmm` alone, because the other two reach `ring`/`zstd-sys` through
`arcbox-image` and the runner has no musl C cross-compiler.

Consequences for edits here:

- A new module that touches `arcbox_hv` (or any Hypervisor.framework
  symbol) needs `#[cfg(target_os = "macos")]` on its `lib.rs` declaration.
  `arcbox-hv` compiles to an EMPTY crate off macOS/aarch64, so an ungated
  consumer fails with `cannot find X in arcbox_hv` — not a missing
  dependency. `dax`, `console_rx_worker`, `net_rx_worker`, and
  `vsock_rx_worker` are all gated for this reason.
- A capability only one backend implements gets a `#[cfg(not(...))]`
  counterpart on `Vmm` that RETURNS `VmmError::Unsupported`, rather than a
  gate the engine layer has to mirror. `set_balloon_target` is the
  reference: the platform difference stops here, and `vm_lifecycle`'s
  balloon controller stays platform-free.
- Local repro (this host's nix toolchain ships no linux-gnu std, so musl
  is the only local Linux target):
  `cargo check --target aarch64-unknown-linux-musl -p arcbox-vmm --all-targets`.
  Use aarch64, not x86_64 — two errors once lived inside a
  `#[cfg(target_arch = "aarch64")]` block in `vmm/linux.rs`.

`vmm/linux.rs` names `KvmHypervisor`/`KvmVm` concretely instead of going
through `arcbox_hypervisor::create_hypervisor()`: it needs `KvmVm`'s
inherent `virtio_devices()` and stores the VM in `Vmm::linux_vm`, neither
of which the erased `impl Hypervisor` return type provides.

## Validation

Follow the ladder in `virt/AGENTS.md`, cheapest first: crate unit tests →
`cargo test -p arcbox-e2e --test hv_vmm -- --ignored` (bare probe) →
`--test virtio_debug` / `--test boot_assets` (each with `-- --ignored`)
under `ARCBOX_VM_BACKEND=hv` (daemon level) → `cargo xtask e2e --repeat N`
for race-class fixes.
