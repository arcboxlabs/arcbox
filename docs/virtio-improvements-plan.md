# VirtIO Improvements — implementation plan

Derived from auditing the current device tree against several mature reference VMMs. Each item below states the gap, the *why* behind the design choice, and the locked fix.

## Phase structure

Phases ordered by risk × effort. Each phase is one PR-sized chunk; each commit inside a phase is atomic (builds + clippy + tests).

---

## Phase 1 — Correctness & security hotfixes

Small, localised commits landing now.

### ✅ Commit 1 — `arcbox-virtio-rng`: fail loudly on entropy failure *(landed: c308159)*

**Why the old behaviour was wrong.** On `getrandom` error the device silently zero-filled the buffer and reported the full length back as valid entropy. A guest kernel has no way to distinguish random bytes from zeroes, so it seeded its CSPRNG with a known-value and downstream TLS/SSH keys were compromised without any signal.

**Locked fix.** Log at warn, break out of the descriptor chain. The used ring carries whatever bytes we actually filled with real entropy; the guest sees a short read and re-requests. `getrandom` is effectively infallible on supported platforms, so the error path is primarily a safety net, not a hot path.

### ✅ Commit 2 — `arcbox-virtio-vsock`: tighten credit-update threshold *(landed: c4a1d1e)*

**Why the old threshold was wrong.** `advance_fwd_cnt` only sent `CREDIT_UPDATE` after 48 KB (3/4 of the 64 KB TX buffer) was drained since the last update. Any guest-side burst between 4 KB and 48 KB exhausts the peer's view of our free buffer before we send them the refresh — TX stalls waiting for a packet we're not sending.

**Locked fix.** Threshold → 4 KB (one page). Granular enough to prevent stall on common burst sizes, coarse enough to avoid spamming credit-only packets. Two tests cover the ≥threshold (enqueue) and <threshold (don't enqueue) cases.

### ✅ Commit 3 — `arcbox-virtio-core`: cap descriptor-chain iteration *(landed: 09a9c89)*

**Why the old behaviour was wrong.** `DescriptorChain::next()` followed `desc.next` unboundedly. A malicious (or buggy) guest can publish a descriptor whose `next` field forms a cycle; iterating the chain spins the host thread forever on every queue kick. This is a one-line denial-of-service from any unprivileged guest code that can write to the descriptor table.

**Locked fix.** Add `ttl: u16` to `DescriptorChain`, initialised to `queue.size` in `pop_avail`. Every legitimate chain has at most `queue.size` descriptors (one per descriptor-table slot), so that's the tightest safe upper bound. Iterator returns `None` once ttl hits zero; caller gets a truncated chain and the guest's malformed request simply fails closed. `queue_guest.rs` already has the equivalent inline cap in `pop_avail` (the `count >= self.size` check).

### Out of scope for Phase 1

- `CREDIT_REQUEST` send path → Phase 2
- Half-close / real state machine → Phase 2
- Everything below

---

## Phase 2 — vsock correctness

### 2.1 CREDIT_REQUEST emission

**Why it matters.** When we want to inject an RW packet but our view of peer `fwd_cnt` is stale, we may believe we have no credit even though the peer has freed buffer. Without a way to ask the peer "what's your current fwd_cnt?", we deadlock waiting for a credit update that won't arrive (the peer only sends one after consuming bytes, but the peer has no pending bytes to consume — both sides wait on each other).

**Locked fix.**
- Add `RxOps::CREDIT_REQUEST = 0x20` (lower priority than `CREDIT_UPDATE`, higher than `RESET`).
- In the RX injection path, before enqueueing an RW op, check `peer_avail_credit() < peer_buf_alloc / 2` and enqueue `CREDIT_REQUEST` on the connection.
- When `CREDIT_REQUEST` dequeues, emit a header-only `VSOCK_OP_CREDIT_REQUEST` packet (len=0). The guest driver responds with `VSOCK_OP_CREDIT_UPDATE`, which our existing TX path already consumes.

### 2.2 Proper connection state machine

**Why `connect: bool` isn't enough.** Half-close (a guest sending `VSOCK_OP_SHUTDOWN` with `no_recv=1, no_send=0`) is a legitimate TCP-like state that lets the peer finish draining before teardown. The current binary flag treats half-close as RESET, dropping pending data. Per-connection timeouts are also impossible to express — a `LocalInit` that never receives a response sits forever.

**Locked fix.**
```rust
enum ConnState {
    LocalInit,                                  // we sent REQUEST, waiting for RESPONSE
    PeerInit,                                   // peer sent REQUEST, we'll send RESPONSE
    Established,                                // bidirectional data flow
    LocalClosed,                                // we sent SHUTDOWN, draining peer→us
    PeerClosed { no_send: bool, no_recv: bool }, // peer sent SHUTDOWN with flags
    Killed,                                     // RST sent/received, cleanup pending
}
```
Plus `expiry: Option<Instant>` for LocalInit (2s) and Killed (2s) — drives a sweep that converts expired connections to `Killed` → `Cleanup`.

---

## Phase 3 — net correctness & perf

### 3.1 MRG_RXBUF: actually implement it

**Why the current code is wrong.** `VIRTIO_NET_F_MRG_RXBUF` is advertised but the RX path stamps `num_buffers=1` on every packet and never spans descriptor chains. Guests that negotiate MRG_RXBUF allocate buffers smaller than frame MTU (the whole point of the feature — small buffer pool for bursty RX); frames exceeding one buffer are either truncated or trigger driver errors.

**Locked fix.** At each RX poll, collect multiple avail chains into an aggregate scatter-gather vector, then issue one `readv` syscall into the combined region. Write the actual chain count used into the vnet header's `num_buffers` field. Push all consumed chains to the used ring in one batch.

### 3.2 TAP offload configuration

**Why the current code is wrong.** We advertise `VIRTIO_NET_F_CSUM` and `GUEST_CSUM` — meaning the guest may send frames with partial checksums expecting the host to complete them. The TAP fd has never been configured to accept such frames; the kernel forwards them with bad checksums, and middleboxes drop them.

**Locked fix.** After feature negotiation, call `tap.set_offload(TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_UFO)` and `tap.set_vnet_hdr_size(size_of::<virtio_net_hdr_v1>() as i32)`. This makes the TAP kernel driver the one that finishes checksums and segments large TSO frames — offloading work to the host network stack where it belongs.

### 3.3 Zero-copy RX

**Why the current code is wrong.** Every RX packet allocates `vec![0u8; 65536]`, copies from the backend fd into that heap buffer, then copies again into guest memory. Two copies per packet plus an allocator round-trip dominates CPU at any sustained rate.

**Locked fix.** Pre-walk avail descriptors to build an `IoVecBufferMut` pointing directly at guest memory, then `readv` the frame straight into the guest. Zero intermediate copies, zero allocations on the hot path.

---

## Phase 4 — fs DAX (mandatory for >90% native target)

**Why it matters.** The in-process FUSE architecture wins on metadata ops by eliminating IPC round-trips (every FUSE_STAT saves a Unix-socket hop vs. a vhost-user daemon). But for large sequential reads, the win evaporates without DAX: we're still copying the file contents through the virtqueue, which is the same cost as vhost-user without DAX would pay. With DAX, the file's page cache is directly mapped into guest GPA and the guest reads with zero kernel involvement — that's the "90% native" path.

**Locked fix.**
- Allocate `MmapRegion` for the DAX window (configurable, default 1 GiB).
- Register the region's GPA with Virtualization.framework via the HV backend.
- Add `FUSE_SETUPMAPPING` / `FUSE_REMOVEMAPPING` opcode handlers.
- Extend `VirtioDevice` with `fn shm_regions() -> &[SharedMemoryRegion]` (default empty).
- Buffer pool per virtqueue thread for FUSE responses (eliminates per-request `Vec<u8>` alloc).

---

## Phase 5 — blk async backend

**Why the current code is wrong.** The live `VirtioBlock::process_queue` uses synchronous `libc::pread`/`pwrite`, blocking the vCPU thread. Three backend structs (`AsyncFileBackend`, `MmapBackend`, `DirectIoBackend`) are defined but disconnected — the abstraction compiles but isn't wired into the hot path. `AsyncFileBackend` additionally reopens the file on every call, which is why it's `#[allow(dead_code)]`.

**Locked fix.**
- Delete `AsyncFileBackend` (structurally broken).
- Pick one live backend per platform: `MmapBackend` on macOS (large pages + `msync`), io_uring on Linux.
- Wire through the existing `AsyncBlockBackend` trait, removing inline pread/pwrite from `device.rs`.
- Add `VIRTIO_BLK_F_DISCARD`, `WRITE_ZEROES`, `TOPOLOGY` handling — container image layer teardown fails today with `Unsupp`.

---

## Phase 6 — macOS `vmnet.framework` backend

**Why the current code is wrong.** `SocketBackend` on P0 (Apple Silicon) routes packets through a UDP tunnel. Double encapsulation overhead plus the kernel UDP stack on both sides puts a hard ceiling well below line rate. `vmnet.framework` provides a shared-memory ring that bypasses the UDP stack entirely, which is the only viable path to ≥10 Gbps on macOS. The entitlements are already in the binary; this is pure engineering.

**Locked fix.** New backend implementing `NetBackend` over `vmnet.framework`: shared ring, `dispatch_queue` for readiness notifications, TSO/GSO offload delegated to the framework. Keep `SocketBackend` available behind a CLI flag for environments where the entitlement isn't granted.

---

## Phase 7 — core foundation

**Why the current code is limiting.** Two design choices from the initial custom-VMM work constrain everything downstream:

1. `GuestMemWriter` holds a single flat `*mut u8` + length. Memory hotplug, MMIO holes below 4 GiB, and ballooning all require discontiguous guest memory; none can be expressed.
2. IRQ injection is an `Arc<dyn Fn(u32)>` closure. No `EventFd`, so there's no path for KVM irqfd injection — every interrupt round-trips through the userspace event loop, bounding high-IOPS block and network devices' latency.

**Locked fix.**
- Adopt a multi-region `GuestMemoryMmap`-style abstraction (the ecosystem has a battle-tested crate; alternative: roll our own multi-region type keeping the same API shape as `GuestMemWriter::slice`).
- Replace both `queue.rs` and `queue_guest.rs` with a single `Queue` type that encapsulates configuration and memory access — devices stop choosing between host-sim and guest-mem paths.
- `EventFd`-based IRQ via a `VirtioInterrupt` trait, wiring to KVM irqfd on Linux and a userspace pipe (acceptable for now; optimise later) on macOS.

This phase is invasive and unlocks nothing on its own — it must follow Phases 3/4/5/6 so their perf work lands first, then they migrate onto the new foundation.

---

## Verification gates (every phase)

1. `cargo build -p <affected crate>` + downstream `cargo build -p arcbox-vmm`.
2. `cargo clippy --all-targets` zero new warnings. Known pre-existing: `len_without_is_empty` at `virt/arcbox-virtio-vsock/src/manager.rs:344`, deferred to Phase 7 cleanup.
3. `cargo test -p <affected crate>`.
4. `arcbox-e2e-test` skill run after Phases 3, 4, 5, 6.
5. Throughput regression check: `iperf3` ≥ 10 Gbps baseline after Phases 3 and 6.
