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

### ✅ 2.1 CREDIT_REQUEST emission *(landed: a2f3402)*

**Why it mattered.** When we want to inject an RW packet but our view of peer `fwd_cnt` is stale, we may believe we have no credit even though the peer has freed buffer. Without a way to ask the peer "what's your current fwd_cnt?", we'd deadlock waiting for a credit update that wouldn't arrive.

**What shipped.**
- `RxOps::CREDIT_REQUEST = 0x20` (after RESET in priority order).
- `VsockConnection::credit_request_pending` flag prevents duplicate requests while one is in flight; cleared on `update_peer_credit`.
- `conn.maybe_request_credit()` called from the RW-send path enqueues `CREDIT_REQUEST` when `peer_avail_credit < peer_buf_alloc / 2` and no request is outstanding.
- The existing zero-credit fallback now also marks the flag via `note_credit_request_sent` so the two paths don't spam duplicate requests.
- Four unit tests cover below-half fires, above-half noop, dedup, and pending-clears-on-peer-response.

### ✅ 2.2 Half-close SHUTDOWN handling *(landed: a52b896)*

**Why it mattered.** `OP_SHUTDOWN` and `OP_RST` shared a match arm that called `remove_connection` regardless of the shutdown flags. A guest doing `shutdown(fd, SHUT_WR)` (half-close on its send side) destroyed the whole connection even though the guest's RX was still active — common TCP-style half-close was broken.

**What shipped.**
- `VSOCK_SHUTDOWN_F_RECEIVE` / `VSOCK_SHUTDOWN_F_SEND` / `VSOCK_SHUTDOWN_F_BOTH` constants.
- New `VsockHostConnections::handle_shutdown(gp, hp, flags)` trait method with a default impl that mirrors `remove_connection` so third-party implementors stay source-compatible.
- `VsockConnectionManager` dispatches on flags: both-bits → teardown; F_RECEIVE only → `conn.mark_peer_no_recv()`; F_SEND only → informational no-op; flags=0 → conservative teardown.
- `VsockConnection::peer_no_recv()` / `accepts_data()` accessors; RX path skips RW for half-closed connections but keeps the fd open so the peer's own sends continue to drain.
- Four unit tests cover F_BOTH, F_RECEIVE, F_SEND, and flags=0.

**Still deferred (not done).** Full `ConnState` enum (`LocalInit` / `Established` / `PeerClosed{no_send, no_recv}` / `Killed`), per-connection expiry, and `PeerInit` peer-originated connections. The `connect: bool` + `peer_no_recv: bool` pair covers the current correctness gap; a richer enum can come when peer-initiated listens land.

---

## Phase 3 — net correctness & perf

### ✅ 3.1 MRG_RXBUF multi-chain RX delivery *(landed: 17a90dd)*

**Why it mattered.** `VIRTIO_NET_F_MRG_RXBUF` was advertised but the RX path only ever wrote into one descriptor chain and never stamped `num_buffers`. Guests that negotiate the feature pre-post small buffers expecting the device to concatenate — frames larger than a single buffer silently truncated.

**What shipped.** `inject_rx_batch` now pops chains until accumulated write-only capacity covers the full frame, stamps the chain count into bytes 10..12 of the first chain's `virtio_net_hdr`, and writes payload across chains with each used-ring entry reporting the correct per-buffer length. Non-MRG_RXBUF guests still get exactly one chain. Two unit tests cover the spanning and single-chain cases.

### ✅ 3.2 TAP offload configuration *(landed: 3c7c73a)*

**Why it mattered.** We advertised `CSUM` / `GUEST_CSUM` / `GUEST_TSO4/6` / `GUEST_UFO` but never called `TUNSETOFFLOAD` on the TAP fd. The Linux kernel TUN driver dropped or mangled partial-checksum and TSO frames that the guest emitted based on our advertisement.

**What shipped.** `NetOffloadFlags` struct + `configure_offload` / `set_vnet_hdr_sz` methods on `NetBackend` trait (default no-op). TAP backend translates `NetOffloadFlags` into `TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN | TUN_F_UFO` and sets `TUNSETVNETHDRSZ = 12`. `VirtioNet::activate` reads `acked_features` and drives both calls.

### ✅ 3.3 RX scratch buffer reuse *(landed: 18d0b92)*

**Why it mattered.** `poll_backend_batch` allocated a fresh `vec![0u8; 65536]` per packet. At sustained rates the allocator dominated CPU even though typical frames are ≤1500 bytes.

**What shipped.** Persistent `rx_scratch: Box<[u8]>` on `VirtioNet`, allocated once in `new()` and reused on every `poll_backend_batch` call. Existing 62 tests still pass unchanged.

**What's explicitly *not* shipped (intentional deferral).** True `readv`-direct-to-guest zero-copy via a `recv_iovec` trait method. The hot path for production RX on the custom VMM is the dedicated net-io worker at `virt/arcbox-vmm/src/net_rx_worker.rs`, which already uses a stack scratch buffer (no heap) and writes directly into guest memory via `GuestMemWriter::slice_mut`. The `poll_backend_batch` path is used by tests, the VZ backend, and the `rx_buffer` staging model; those don't justify the trait-level API restructure today. Revisit when a production caller wants the readv path — at which point the locked design (extend `NetBackend` with a default-copy `recv_iovec`, TAP-override with `readv`, call from `inject_rx_batch`) still applies.

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
