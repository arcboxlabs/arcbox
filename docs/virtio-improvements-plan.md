# VirtIO Improvements — implementation plan

Source of truth for gaps: comparison against Firecracker, rust-vmm, and Cloud Hypervisor done on `feat/custom-vmm-phase2`. Reference clones at `/Users/Shiro/Developer/vmm-research/`.

## Phase structure

Phases ordered by risk × effort. Each phase is one PR-sized chunk; each commit inside a phase is atomic (builds + clippy + tests).

---

## Phase 1 — Correctness & low-effort hotfixes (this plan)

Three small commits. Locked scope:

### Commit 1 — `arcbox-virtio-rng`: fail loudly on entropy failure

**Problem.** `virt/arcbox-virtio-rng/src/lib.rs:157-161` silently zero-fills on `getrandom` error and still reports full `filled` bytes to the guest. Security defect: guest receives all-zero "entropy" thinking it's random.

**Fix.** On `getrandom` failure:
- Log at `tracing::warn!` with the error.
- Stop filling this descriptor chain (do **not** increment `filled` for the failed chunk).
- Complete the chain via the used ring with whatever `filled` bytes were actually written with valid entropy (honest short read). Guest kernel re-requests.

No new dependencies. No `Result` plumbing needed — failure path stays local to the chain-walking loop.

**Verification.** Build + clippy + existing rng tests pass. No new test required (error path is not triggerable without stubbing `getrandom`).

### Commit 2 — `arcbox-virtio-vsock`: credit-update threshold 48 KB → 4 KB

**Problem.** `virt/arcbox-virtio-vsock/src/manager.rs:185` triggers `CREDIT_UPDATE` only after the host has consumed `TX_BUFFER_SIZE * 3/4 = 48 KB` since the last credit packet. Firecracker uses 4 KB. Coarse threshold → guest TX stalls on bursts between 4 KB and 48 KB.

**Fix.** Define `pub const CREDIT_UPDATE_THRESHOLD: u32 = 4096;` next to `TX_BUFFER_SIZE` and replace the `TX_BUFFER_SIZE * 3 / 4` expression at line 185 with `CREDIT_UPDATE_THRESHOLD`.

**Verification.** Existing test at `manager.rs:572` (`fwd_cnt_triggers_credit_update`, which writes 50 KB) still passes unchanged (50 KB > 4 KB). Build + clippy + full vsock test suite.

### Commit 3 — `arcbox-virtio-core`: descriptor-chain TTL

**Problem.** `DescriptorChain` in `virt/arcbox-virtio-core/src/queue.rs:317` follows `next` pointers unboundedly. A malformed guest ring (circular `next` chain) spins forever in the host. Firecracker and rust-vmm both cap traversal via a TTL counter.

**Fix.** Add `ttl: u16` to `DescriptorChain`, initialized to `queue.size` in `pop_avail()`. In `Iterator::next()`, decrement `ttl` before following `desc.has_next()`; when `ttl == 0`, return `None` (graceful chain termination, no panic). Same fix applies to `GuestMemoryVirtQueue`'s chain iterator in `queue_guest.rs` if it has the same shape — verify during implementation; if not, it already reads a pre-collected `Vec` so no loop is possible.

**Verification.** Existing `queue.rs` tests pass. Add one new unit test: build a VirtQueue with a self-cycling descriptor (`desc[0].next = 0`), call `pop_avail()`, iterate the chain, assert iteration terminates in at most `queue.size` steps.

### Out of scope for Phase 1

- vsock `CREDIT_REQUEST` send path (new `RxOps` variant + wire emission + trigger logic) → Phase 2
- vsock half-close / proper state machine → Phase 2
- Everything in Phases 2+ below

### Phase 1 rollback

Each commit is independent. Revert any single commit without affecting the others.

---

## Phase 2 — vsock correctness (state machine + CREDIT_REQUEST)

Not implemented yet. Locked scope on implementation:

- Add `VSOCK_OP_CREDIT_REQUEST` to `RxOps` enum as `CREDIT_REQUEST = 0x20` (lower priority than CREDIT_UPDATE).
- In the RX injection path where RW packets are built, check `peer_avail_credit() < peer_buf_alloc / 2` and enqueue `CREDIT_REQUEST` on the connection before enqueuing the RW op.
- Emit a VSOCK_OP_CREDIT_REQUEST packet (header-only, len=0) when this op dequeues.
- Replace `connect: bool` with `enum ConnState { LocalInit, PeerInit, Established, LocalClosed, PeerClosed { no_send: bool, no_recv: bool }, Killed }` matching Firecracker's state machine.
- Add per-connection `expiry: Option<Instant>` for LocalInit (2s timeout) and Killed.

---

## Phase 3 — net correctness (MRG_RXBUF + TAP offload)

- Implement proper MRG_RXBUF: aggregate multiple avail chains into one `IoVecBufferMut`, single `readv` into guest memory, write `num_buffers` into each consumed chain's vnet header.
- Call `tap.set_offload(TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_UFO)` after feature negotiation in `TapBackend::activate` (or equivalent).
- Call `tap.set_vnet_hdr_size(size_of::<virtio_net_hdr_v1>() as i32)`.
- Remove per-packet heap allocation: pre-allocate RX buffer on device struct, reuse across polls.

---

## Phase 4 — fs DAX (mandatory for >90% native target)

- Allocate `MmapRegion` for DAX window (configurable size, default 1 GiB).
- Register region GPA with Virtualization.framework via the HV backend.
- Add `FUSE_SETUPMAPPING` and `FUSE_REMOVEMAPPING` opcode handlers in `session.rs` / `handler.rs`.
- Export shared-memory descriptor via a new `VirtioDevice::shm_regions()` method (default empty).
- Add buffer pool for FUSE response allocation (per virtqueue thread).

---

## Phase 5 — blk async backend

- Delete dead `AsyncFileBackend` (reopens file per call, broken).
- Pick one live backend: `MmapBackend` on macOS, io_uring-based on Linux.
- Wire via `AsyncBlockBackend` trait (already defined), removing inline `libc::pread`/`pwrite` from `device.rs`.
- Add `VIRTIO_BLK_F_DISCARD`, `VIRTIO_BLK_F_WRITE_ZEROES`, `VIRTIO_BLK_F_TOPOLOGY` handling.

---

## Phase 6 — macOS `vmnet.framework` backend (P0 platform perf)

- Replace `SocketBackend` UDP tunnel with a real `vmnet.framework` integration (shared-memory ring, entitlements already present).
- Target: ≥10 Gbps on P0, reach for 50 Gbps on Apple Silicon with TSO offload.

---

## Phase 7 — core foundation

- Adopt rust-vmm `vm-memory` crate: multi-region `GuestMemoryMmap` replaces `GuestMemWriter`. Blocks memory hotplug until done.
- Adopt rust-vmm `virtio-queue` crate: replace both `queue.rs` and `queue_guest.rs` with a single `Queue` type.
- `EventFd`-based IRQ injection (`VirtioInterrupt` trait like Cloud Hypervisor), wiring to KVM irqfd on Linux and a userspace pipe on macOS.

---

## Verification gates (every phase)

1. `cargo build -p <affected crate>` + downstream `cargo build -p arcbox-vmm`.
2. `cargo clippy --all-targets` for affected crates — zero new warnings. Pre-existing `len_without_is_empty` at `virt/arcbox-virtio-vsock/src/manager.rs:344` is acknowledged noise and may be left until Phase 7 cleanup.
3. `cargo test -p <affected crate>`.
4. End-to-end `arcbox-e2e-test` skill run after Phases 3, 4, 5, 6 (perf-affecting phases).
5. `iperf3 ≥ 10 Gbps` regression check after Phases 3 and 6.

## Priority rationale

Phases 1–2 are correctness (guest-visible contract violations or security defects). Phases 3–6 are perf unlocks directly blocking the project's stated targets (>50 Gbps net, >90% native fs). Phase 7 is structural — it removes the two largest pieces of hand-rolled code and brings ArcBox onto the mainstream rust-vmm stack, but it's invasive and does not unblock any performance target until Phases 3–6 are in place.
