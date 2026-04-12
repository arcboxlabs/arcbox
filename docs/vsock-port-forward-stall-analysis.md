# Vsock Port Forwarding Stall Analysis

## Problem

Vsock-based port forwarding stalls after ~2 seconds under sustained high-throughput load.
iperf3 sender reports ~5-11 Gbps for the first 1-2 seconds, then drops to 0.
**receiver = 0 bytes** — data enters the guest but never reaches the application.
Guest kernel reports RCU stall warnings.

Reproducible on every run with `iperf3 -c 127.0.0.1 -p 5201 -P 4 -t 10`.

## Architecture

```
Host iperf3 client
  → Host TCP (127.0.0.1:5201)
    → VsockPortForwarder accepts TCP, opens vsock to guest:1025
      → 6-byte handshake (target IP + port)
      → copy_bidirectional (host TCP ↔ vsock socketpair)
        → vsock_rx_worker: reads socketpair → builds VsockHeader → injects to guest RX virtqueue
          → Guest kernel virtio_vsock driver (softirq)
            → Guest vsock socket
              → Guest agent splice relay (vsock fd → pipe → container TCP fd)
                → Container iperf3 server
```

Host→guest data path (the bottleneck direction):
1. Host TCP read → socketpair write (tokio async relay)
2. Worker kqueue wakes → `libc::read(socketpair_fd)` → build OP_RW packet (44-byte VsockHeader + payload)
3. `inject_packet()` writes to guest RX virtqueue descriptor
4. `trigger_irq()` → GIC SPI + `hv_vcpus_exit()` → guest processes interrupt
5. Guest `virtio_vsock_rx_done()` → dequeue all descriptors → deliver to vsock socket
6. Guest agent splice relay → container TCP

## Symptoms

1. **Sender ~5-11 Gbps for 1-2 seconds, then 0** (all 4 parallel streams stall simultaneously)
2. **Receiver = 0 bytes** (iperf3 server inside container receives nothing)
3. **Guest kernel RCU stall** (`rcu: INFO: rcu_sched detected stalls on CPUs/tasks`)
4. After stall, guest becomes unresponsive (docker exec hangs, no console output)
5. Problem persists across daemon restarts if VM stays alive (stale state)

## Root Causes Found and Fixed

### 1. peer_avail_credit() u32 wrapping underflow (CVE-2026-23069 equivalent)

**File**: `virt/arcbox-vmm/src/vsock_manager.rs:183`

**Before (buggy)**:
```rust
pub fn peer_avail_credit(&self) -> usize {
    (Wrapping(self.peer_buf_alloc) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
}
```

When `in_flight = rx_cnt - peer_fwd_cnt > peer_buf_alloc`, the `Wrapping<u32>` subtraction wraps to ~4GB instead of returning 0. The host believes it has unlimited credit and floods the guest.

**After (fixed)**:
```rust
pub fn peer_avail_credit(&self) -> usize {
    let in_flight = (self.rx_cnt - self.peer_fwd_cnt).0;
    self.peer_buf_alloc.saturating_sub(in_flight) as usize
}
```

Inner counter difference `rx_cnt - peer_fwd_cnt` correctly uses `Wrapping` (monotonic counters that wrap at u32). Outer subtraction from `peer_buf_alloc` uses `saturating_sub` to prevent underflow.

**Evidence from logs**:
```
buf_alloc=262144 rx_cnt=652774727 peer_fwd=652410183 → in_flight=364544 > 262144
buf_alloc=262144 rx_cnt=644637575 peer_fwd=644211591 → in_flight=425984 > 262144
```
Under the old code, these produce credit ≈ 4,293,132,416 instead of 0.

**Regression tests**: 3 tests added covering normal operation, saturation, and wrapping counter edge cases.

### 2. fwd_cnt regression from out-of-order TX packets

**File**: `virt/arcbox-vmm/src/vsock_manager.rs:191`

`update_peer_credit()` unconditionally overwrote `peer_fwd_cnt` with the value from each guest TX packet. If multiple TX packets were processed in one `process_queue` call, a later packet with an older `fwd_cnt` could regress the value.

**Fix**: Only accept `fwd_cnt` that advances (wrapping-safe comparison using half-space rule):
```rust
let new = Wrapping(fwd_cnt);
if (new - self.peer_fwd_cnt).0 < (1u32 << 31) {
    self.peer_fwd_cnt = new;
}
```

### 3. buf_alloc regression from out-of-order TX packets

Same file. `update_peer_credit()` unconditionally overwrote `peer_buf_alloc`. A guest OP_RW packet with default `buf_alloc=262144` could overwrite the SOL_VSOCK-configured value of `2097152`.

**Fix**: Only accept larger `buf_alloc`:
```rust
if buf_alloc > self.peer_buf_alloc {
    self.peer_buf_alloc = buf_alloc;
}
```

## Remaining Problem: Guest RCU Stall

Even with all credit fixes, the stall persists. The credit fixes are **necessary but not sufficient**.

### What happens after the credit fix

1. Credit calculation is now correct (verified: 0 CREDIT OVERRUN in logs)
2. Host respects the 262KB (or 2MB with SOL_VSOCK) credit window
3. But host still injects data at ~5-11 Gbps in bursts
4. Guest `virtio_vsock_rx_done()` callback processes ALL available descriptors in one shot
5. Guest softirq runs for too long → RCU grace-period kthreads starve → RCU stall

### RCU stall trace from guest

```
[  89.929610] rcu: INFO: rcu_sched detected stalls on CPUs/tasks:
[  89.929696] rcu: 	0-...0: (97 ticks this GP) idle=b68c/1/0x4000000000000002 softirq=347/349 fqs=266
[  89.929740] rcu: 	1-...0: (70 ticks this GP) idle=4d54/1/0x4000000000000000 softirq=386/387 fqs=266
```

Key observations:
- CPUs 0 and 1 have **pending softirqs** (347/349, 386/387) — the RX processing softirq is running
- `fqs=266` — grace-period kthread has been trying to advance for 266 force-quiescent-state iterations
- This matches the Linux RCU stall documentation: "Anything that prevents RCU's grace-period kthreads from running"

### Why virtio_vsock lacks NAPI budgeting

Linux's virtio-net driver uses NAPI with a per-poll budget (default 64 packets via `net.core.netdev_budget`). After processing `budget` packets, the driver **yields to let other work run**.

The virtio_vsock driver does NOT use NAPI. Its RX processing in `virtio_transport.c` processes all available descriptors in a work queue callback without any budget limit:

```c
// net/vmw_vsock/virtio_transport.c (simplified)
static void virtio_vsock_rx_done(struct virtqueue *vq) {
    schedule_work(&vsock->rx_work);  // deferred to work queue
}

// In the work handler:
static void virtio_transport_rx_work(struct work_struct *work) {
    while ((buf = virtqueue_get_buf(vq, &len)) != NULL) {
        virtio_transport_recv_pkt(buf);  // no budget limit
    }
}
```

Compare with virtio-net:
```c
static int virtnet_poll(struct napi_struct *napi, int budget) {
    while (received < budget) {  // stops after budget packets
        buf = virtqueue_get_buf(vq, &len);
        if (!buf) break;
        receive_buf(vi, rq, buf, len);
        received++;
    }
    if (received < budget)
        napi_complete_done(napi, received);  // yield CPU
    return received;
}
```

### Attempted mitigations (all insufficient)

| Mitigation | Result | Why insufficient |
|------------|--------|-----------------|
| Reduce BATCH_SIZE (128→16) | Stall persists | Host injects fewer packets per wakeup, but guest still processes all available at once |
| Add 50μs yield after batch | Stall persists | Yield is on the host thread, doesn't affect guest softirq scheduling |
| EVENT_IDX notification suppression | Stall persists | Guest vsock driver may not properly use EVENT_IDX for RX |
| Remove `hv_vcpus_exit()` | **Worse** — WFI deadlock | vCPUs need explicit kick to exit WFI on Apple HVF; GIC SPI alone insufficient |
| Credit-notify pipe | Stall persists + busy-loop | Wrong optimization direction; made things worse |

### What would actually fix this

**Option A: Guest kernel patch** — Add NAPI-style budgeting to `virtio_transport_rx_work()` in `net/vmw_vsock/virtio_transport.c`. Process at most N descriptors per work iteration, then reschedule. This is the correct long-term fix but requires maintaining a kernel patch.

**Option B: Host-side admission control** — After injecting a batch, don't inject more until the guest has processed the previous batch (evidenced by a CreditUpdate). This effectively rate-limits the host to the guest's processing capacity. Challenge: the CreditUpdate comes via the TX queue which is processed on the vCPU thread, not the worker thread.

**Option C: Revert to virtio-net for port forwarding** — The virtio-net path (smoltcp + frame injection) has NAPI on the guest side and was stable at 10.3 Gbps. The vsock architecture eliminates the double TCP stack but introduces the NAPI-less RX processing bottleneck.

**Option D: Hybrid approach** — Use virtio-net for bulk data transfer (with existing MRG_RXBUF + GSO + inline inject infrastructure), vsock only for low-throughput control channels (agent RPC, Docker API proxy). Port forwarding reverts to the virtio-net path.

## Test Environment

- Host: macOS Apple Silicon, Hypervisor.framework
- Guest: Custom Linux kernel (6.x series, aarch64)
- VMM: ArcBox custom Rust VMM with HV backend
- VirtIO transport: MMIO (not PCI)
- vsock device: Custom implementation with FEATURE_STREAM + FEATURE_VERSION_1 + VIRTIO_F_EVENT_IDX
- Worker thread: Dedicated OS thread with kqueue-based fd polling
- Guest agent: Rust (tokio runtime), splice relay for port forwarding

## Files

| File | Role |
|------|------|
| `virt/arcbox-vmm/src/vsock_rx_worker.rs` | Host-side kqueue worker thread (RX injection) |
| `virt/arcbox-vmm/src/vsock_manager.rs` | Connection manager + credit tracking (**fix here**) |
| `virt/arcbox-vmm/src/device.rs` | VirtIO device MMIO handling, worker spawn |
| `virt/arcbox-virtio/src/vsock.rs` | VirtIO vsock device model (TX/RX queue processing) |
| `virt/arcbox-port-forward/src/forwarder.rs` | Host TCP→vsock relay |
| `guest/arcbox-agent/src/port_forward.rs` | Guest vsock→TCP relay (splice) |

## Reproduction

```bash
# Build and deploy
cargo build -p arcbox-daemon --features vmnet
cargo build -p arcbox-agent --target aarch64-unknown-linux-musl --release
codesign --force --options runtime \
    --entitlements bundle/arcbox.entitlements \
    -s "Developer ID Application: ArcBox, Inc. (422ACSY6Y5)" \
    target/debug/arcbox-daemon
cp target/aarch64-unknown-linux-musl/release/arcbox-agent ~/.arcbox/bin/arcbox-agent

# Start daemon (fresh VM — kill any existing daemon and wait for VM shutdown)
pkill -f arcbox-daemon; sleep 15
RUST_LOG=info target/debug/arcbox-daemon &
sleep 35

# Start iperf3 server in container
DOCKER_HOST=unix://$HOME/.arcbox/run/docker.sock \
    docker run -d --name iperf-server -p 5201:5201 networkstatic/iperf3 -s
sleep 5

# Reproduce the stall (stalls at ~2 seconds, receiver=0)
iperf3 -c 127.0.0.1 -p 5201 -P 4 -t 10

# Check host-side diagnostics
grep "CREDIT OVERRUN" ~/.arcbox/log/daemon.log        # should be 0
grep "credit=0" ~/.arcbox/log/daemon.log | tail -5     # credit exhaustion events
grep "rcu.*stall" ~/.arcbox/log/daemon.log | tail -5   # guest RCU stall

# Run unit tests (credit underflow regression tests)
cargo test -p arcbox-vmm --lib vsock_manager
```

**Expected**: stalls after ~2 seconds, `receiver=0`, guest RCU stall in logs.

**Key diagnostic**: if `CREDIT OVERRUN` appears, the `saturating_sub` fix regressed.
If `credit=0` never appears but stall still happens, the guest is overwhelmed before credit even depletes.

## References

- [Linux RCU stall documentation](https://docs.kernel.org/RCU/stallwarn.html)
- [CVE-2026-23069 — virtio_vsock credit underflow](https://nvd.nist.gov/vuln/detail/CVE-2026-23069)
- [Linux kernel fix commit 60316d7f10b17a7](https://github.com/torvalds/linux/commit/60316d7f10b17a7ebb1ead0642fee8710e1560e0)
- [VirtIO spec 2.7.7.2 — Used Buffer Notification Suppression](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html)
