# Network datapath — measured performance and known limits

As of `net-perf-mergeable-rx` tag (commit `eb45a46`, 2026-04-24). Host: Apple Silicon, macOS 26.0.1. Guest: Linux under HV backend. Bench: `iperf3` client on host against `networkstatic/iperf3` container in guest, port-forwarded.

## Headline numbers

| Direction | Single stream | `-P 4` | VZ reference |
|---|---|---|---|
| Host → VM | **22.7 Gbps** | (see below) | ~10 Gbps |
| VM → Host | **15.0 Gbps** | **19.1 Gbps** (0 retrans) | ~10 Gbps |

Single-stream exceeds Apple's native VZ virtio-net by ~2×. See `virt/arcbox-net-inject/src/inject.rs` — the inject thread uses `VIRTIO_NET_F_MRG_RXBUF` with a vectored `readv()` across up to 16 descriptors per syscall, so one GSO_TCPV4 frame fans out into 40+ MSS-sized segments on the guest.

## Known limitation: Host → VM collapses under multi-flow saturation

When multiple parallel Host → VM flows each try to push above ~5 Gbps at once, throughput collapses. One flow usually survives near full speed; the others drop to near-zero at the receiver. Measurement at `-P 2` with rate caps (`iperf3 -b`):

| Rate cap per flow | Receiver total | Per-flow status |
|---|---|---|
| `-b 500M` | 1.0 Gbps | both clean |
| `-b 2G` | 4.0 Gbps | both clean |
| `-b 4G` | 6.6 Gbps | 17% loss on one flow |
| `-b 6G` | 6.0 Gbps | one flow full, **other ≈ 0** |
| `-b 8G+` | ≈ 1 Gbps | one flow survives, rest drop |
| `-b 2G` with `-P 4` | 8.0 Gbps | all four clean |

Unbounded `-P 2/3/4` at line rate reproduces the collapse. The per-flow breaking point is roughly 4–5 Gbps each.

## What we've ruled out

- **Inject scheduler fairness.** Both the serial drain-per-conn and a round-robin across conns produce the same collapse under saturating load. The hot loop isn't starving conns.
- **RX ring capacity.** `QUEUE_NUM_MAX` is already 1024 (`virt/arcbox-vmm/src/device/mmio_state.rs:163`); bumping the inject-side caps didn't change the failure mode.
- **Inject CPU.** The host-side inject thread is nowhere near pegged during these tests.

## What we haven't checked yet

Three cheap guest-side experiments to run before considering structural changes (multi-queue virtio-net, new backend, etc.):

1. **Raise `QUEUE_NUM_MAX` from 1024 to 2048** and rerun `-P 2 -b 6G`. If the picture changes (both flows get data but still lossy), ring capacity/fairness is real. If unchanged, the bottleneck is deeper.
2. **Guest `ethtool -S` counters** (`rx_missed`, `rx_no_buffer`) + **`/proc/interrupts` CPU distribution** during `-P 2 -b 6G`. If all virtio-input IRQs land on one guest CPU, multi-queue is justified. If CPU distribution is fine, look elsewhere (GRO, softirq budget).
3. **Guest softirq %** during `-P 2 -b 4G`. If `%si` is pegged on one CPU, it's a NAPI-budget problem — try tuning `netdev_budget` / `netdev_budget_usecs` inside the guest before any structural work.

Pick the fix based on which of those three actually moves the needle. Multi-queue virtio-net is substantial (frontend, backend N threads, feature negotiation, flow hashing) and should not be speculated into.

## Reproducer

```bash
# Start a clean daemon.
arcbox daemon start

# Run an iperf3 server in a guest container.
docker run -d --rm --name iperf3-srv -p 5201:5201 networkstatic/iperf3 -s

# Headline tests.
iperf3 -c 127.0.0.1 -p 5201 -t 10                  # single stream (expect ≥20 Gbps)
iperf3 -c 127.0.0.1 -p 5201 -R -t 10               # VM→Host (expect ≥15 Gbps)
iperf3 -c 127.0.0.1 -p 5201 -R -P 4 -t 10          # VM→Host parallel (expect ≥15 Gbps sum)

# Collapse reproducer (each flow 6 Gbps — expect one survives, others drop).
iperf3 -c 127.0.0.1 -p 5201 -P 2 -b 6G -t 10

# Rate-limited works clean.
iperf3 -c 127.0.0.1 -p 5201 -P 4 -b 2G -t 10
```
