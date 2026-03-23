# Network Stack Audit Issues

Comprehensive audit of the outbound network stack (`virt/arcbox-net/src/darwin/`).
Performed 2026-03-23. Issues sorted by priority.

## Fixed (PR #97)

- [x] **CRITICAL** `tcp_bridge.rs` — Concurrent SYNs to same port share single listen socket; `docker compose` parallel pulls get RST
- [x] **HIGH** `tcp_bridge.rs` — Listen socket memory leak: unconsumed LISTEN sockets (512 KiB each) accumulate in `port_handles` indefinitely
- [x] **MEDIUM** `tcp_bridge.rs` — `resolve_proxy_target` dead-code guard + HTTPS proxy only used for port 443 (HTTP CONNECT works on any port)
- [x] **MEDIUM** `datapath_loop.rs` — `forward_dns_async` accepts response without validating DNS transaction ID
- [x] **LOW** `socket_proxy.rs`, `inbound_relay.rs` — `Instant::checked_sub(60s).unwrap()` panics if daemon starts within 60 s of system boot

---

## P0 — Data Correctness

### Partial frame write causes data loss
- **Severity**: CRITICAL
- **File**: `datapath_loop.rs:221-226, 751-757`
- **Issue**: `fd_write()` can return a short write (`n < frame.len()`), but the caller unconditionally pops the entire frame from `write_queue`. The remaining bytes are silently discarded, corrupting the L2 frame delivered to the guest.
- **Fix**: Track bytes-written per queued frame; only pop after full write. Alternatively, slice the frame on partial write and re-queue the remainder.

### DNS reply starvation under sustained guest traffic
- **Severity**: HIGH
- **File**: `datapath_loop.rs:207, 273, 344, 680-688`
- **Issue**: When `write_queue.len() >= WRITE_QUEUE_HIGH` (512), the `if accept_replies` guard completely blocks the `reply_rx.recv()` arm in `select!`. The fallback `drain_reply_rx()` in the common tail only drains 64 frames (non-blocking). Under sustained guest traffic with a full write queue, DNS reply tasks block on `dns_reply_tx.send()`, causing DNS responses to be lost or indefinitely delayed.
- **Fix**: Remove the `if accept_replies` guard, or make `drain_reply_rx()` unbounded when backpressure is active.

### Upstream DNS detection includes loopback and fake-IP addresses
- **Severity**: HIGH
- **File**: `dns.rs:103-137`
- **Issue**: `parse_resolv_conf_nameservers` accepts any IP from `/etc/resolv.conf` without filtering. On macOS, mDNSResponder sets `nameserver 127.0.0.1`; Surge/Clash set `nameserver 198.18.0.2`. Forwarding to loopback may loop or fail; forwarding to fake-IP returns unusable addresses.
- **Fix**: Filter `127.0.0.0/8`, `::1`, and `198.18.0.0/15` from detected upstream servers. Fall back to `DEFAULT_UPSTREAM` (8.8.8.8, 1.1.1.1) when all detected servers are filtered.

---

## P1 — Resource Leaks / Robustness

### UDP flow removal does not signal spawned task
- **Severity**: HIGH
- **File**: `socket_proxy.rs:94-103`
- **Issue**: When `try_send()` fails, the UDP flow is removed from the HashMap but the spawned relay task (line 119) keeps running — it waits on `payload_rx.recv()` which never returns `None` because the `payload_tx` was dropped implicitly (HashMap entry removed). The task survives until its 60 s recv timeout.
- **Fix**: Explicitly drop `payload_tx` when removing the flow, or store and abort the `JoinHandle`.

### Spawned tasks lack CancellationToken — orphaned on shutdown
- **Severity**: HIGH
- **File**: `socket_proxy.rs:119, 247`; `datapath_loop.rs:529, 544`
- **Issue**: All spawned tasks (UDP relay, ICMP proxy, DNS forwarding) have no cancellation mechanism. When the datapath loop exits, these tasks become orphans and run until their individual timeouts expire (up to 60 s). During this window they hold sockets, buffers, and channel handles.
- **Fix**: Propagate the existing `CancellationToken` (used by the datapath loop) into all spawned tasks via `tokio::select!` with `cancel.cancelled()`.

### DNS response parser: answer-section name skip lacks bounds check
- **Severity**: MEDIUM
- **File**: `dns_log.rs:164-180`
- **Issue**: The name-skipping loop in the answer section advances `offset += 1 + len` without verifying `offset + 1 + len <= data.len()`. A malformed DNS response with an invalid label length can cause an out-of-bounds read or silent offset corruption.
- **Fix**: Add `if offset + 1 + len > data.len() { break; }` before advancing.

### DNS log ignores AAAA-only responses
- **Severity**: MEDIUM
- **File**: `dns_log.rs:207-211`
- **Issue**: `parse_dns_response_a_records` returns `None` when a response contains only AAAA records (no A records). IPv6-only domains are never recorded in the DNS log, so `TcpBridge` cannot resolve their domain name for proxy-aware CONNECT tunneling.
- **Fix**: Extend the parser to also extract AAAA records (type 28), or at minimum document the IPv4-only limitation.

---

## P2 — Performance / Edge Cases

### DNS cache is a stub
- **Severity**: MEDIUM
- **File**: `dns.rs:579-588`
- **Issue**: `cache_response` stores `records: Vec::new()` — the cache infrastructure exists but never actually caches response data. Every DNS query hits the upstream server unconditionally.
- **Fix**: Implement proper response parsing in `cache_response`, respecting per-record TTLs.

### DNS response source IP not validated
- **Severity**: MEDIUM
- **File**: `datapath_loop.rs:606`
- **Issue**: `forward_dns_async` calls `socket.recv_from()` but discards the source address (`_`). A stale or spoofed UDP packet from any IP is accepted as a valid DNS response. Practical risk is low because each call creates a fresh socket on a random ephemeral port, but it violates defense-in-depth.
- **Fix**: Validate that the response source matches the upstream server address.

### DNS log LRU eviction is not true LRU
- **Severity**: MEDIUM
- **File**: `dns_log.rs:62-68`
- **Issue**: When the log reaches `MAX_ENTRIES` (4096), eviction only removes expired entries (TTL 300 s). If all 4096 entries are still fresh, new entries are silently dropped — `record()` inserts into a full map without evicting the oldest.
- **Fix**: Implement true LRU eviction (remove the oldest entry) when capacity is reached, regardless of TTL.

### Ephemeral port allocator has no collision detection
- **Severity**: LOW
- **File**: `inbound_relay.rs:61-68`
- **Issue**: The wrapping port allocator reuses ports after `EPHEMERAL_END`. If a previous flow's task is still running (within its 60 s timeout), the reused port creates a duplicate flow key. Reply routing becomes ambiguous.
- **Fix**: Check `udp_flows.contains_key()` before inserting, or skip to the next port on collision.

### ICMP proxy: per-request socket and 65 KB buffer allocation
- **Severity**: LOW
- **File**: `socket_proxy.rs:247-292`
- **Issue**: Each ICMP echo request spawns a new tokio task, creates a new ICMP datagram socket, and heap-allocates a 65 KB receive buffer. No flow tracking, no socket reuse. High-frequency pings cause significant allocation churn.
- **Fix**: Reuse a single ICMP socket with echo ID-based demultiplexing, or implement a buffer pool.

### ICMP proxy permanently disabled on first PermissionDenied
- **Severity**: LOW
- **File**: `socket_proxy.rs:257-264`
- **Issue**: A single `PermissionDenied` error sets `disabled = true` permanently. All subsequent ICMP packets are silently dropped with no recovery path and no operator-visible indication beyond a one-time log warning.
- **Fix**: Implement periodic retry (e.g., every 60 s) instead of permanent disable, or surface the error to the health check endpoint.
