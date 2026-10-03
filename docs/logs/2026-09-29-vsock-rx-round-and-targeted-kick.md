# 2026-09-29 — vsock RX: drain a stream per round, aim the kick

- Type: change + measurement
- Area: HV backend, `arcbox-virtio-vsock`, `arcbox-vmm`
- Commits: 5de23af4, e2b6d3ad, dc5b48de (e2e seed fix), 57592261 (docs)
- Superseded in part by ADR 0001 (the targeted kick is gone; the round
  drain stays)

## Problem

On HV, `head -c 1073741824 /dev/zero | docker run -i --rm alpine wc -c`
took 9–14 s at low host load and 20–30 s under load (VZ: 8–9 s). A
`GetVirtioDebug` diff across one transfer: `kickBroadcasts` +288 k,
`unparkBroadcasts` +349 k, vCPU `kicksReceived` +3.39 M. `sample` put ~43%
of the `vsock-io` thread in `hv_vcpus_exit` and its locks. The guest sat
95% idle: the cost was entirely host-side, one interrupt and one 18-vCPU
kick per ≤3776-byte packet.

## Root cause

`poll_rx_injection` wrote one packet per connection per round: `RxOps::RW`
is a bitmask flag, cleared on dequeue, so after one packet the connection
was no longer pending and the round ended. The worker then raised
INT_VRING and broadcast `hv_vcpus_exit` once per round, i.e. per packet.
1 GiB / 3776 B ≈ 284 k packets, matching the counter.

## Changes

1. Keep `RW` pending while each read fills the packet, so a round drains
   the stream as far as credit and posted RX descriptors allow,
   round-robin across connections; `rxq_starved` semantics unchanged.
   `RxOps::CREDIT_REQUEST` re-ranked above `RW` so the half-window credit
   refresh is not starved behind the data.
2. Kick only the vCPU that last wrote `INTERRUPT_ACK` for the vsock device
   (Linux routes every virtio SPI to CPU 0; `/proc/interrupts` in the guest
   confirmed it), falling back to all until the first ACK. **Removed the
   next day**: no kick is needed at all (ADR 0001).
3. e2e harness: `boot-assets/dev/runtime-bin` seeds into
   `runtime/<version>/`, the directory the daemon reads.

## Results (HV, 18 vCPUs, host load 25–35 both before and after)

| Metric | master 0a93f43a | after |
|---|---|---|
| 1 GiB pipe ×3 | 20.49 / 19.60 / 19.14 s | 5.96 / 5.74 / 5.55 s |
| `kickBroadcasts` Δ | +290 619 / +289 297 / +289 953 | +256 / +164 / +165 |
| `unparkBroadcasts` Δ | +363 863 / +361 163 / +356 049 | +20 632 / +19 929 / +19 478 |
| vCPU `kicksReceived` Δ (sum) | +3.51 M / +3.53 M / +3.24 M | +9 784 / +7 684 / +7 390 |
| `docker version` during `cat /dev/zero \| docker run -i … sleep 30` | 0.01–0.02 s | 0.01–0.02 s |

VZ (same build, regression check only): pipe 4.27 / 4.79 / 4.66 s, `docker
load` 300 MB 1.38–1.71 s, build/attach/stdin-EOF fine. `docker load`
300 MB on HV: 3.24 / 4.32 / 3.74 s. e2e smoke passed (VmStarting→VmReady
3.7 s). Full workspace tests: one load-induced flake
(`arcbox-core` `listeners_follow_the_published_ports`, passes 3/3 alone).

## Method notes

- Counter diff: `grpcurl -plaintext -unix -import-path
  rpc/arcbox-protocol/proto -proto api.proto <sock>
  arcbox.v1.SystemService/GetVirtioDebug` before and after; queue 0's
  `index` is omitted in JSON, match `(.index//0)==0`.
- `docker attach` tests need `--sig-proxy=false`; `timeout` forwards
  SIGTERM into the container and the attach never returns.
- The e2e smoke on a host that cannot reach the CDN needs
  `boot-assets/dev/{kernel,rootfs.erofs,manifest.json}` and `runtime-bin/`
  staged from a bundle of the pinned version.
