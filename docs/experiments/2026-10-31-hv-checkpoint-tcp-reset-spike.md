# 2026-10-31 — Can an HV checkpoint save its TCP flow table so the restore resets flows the guest is silently waiting on?

- Type: experiment
- Area: `virt/arcbox-net` (`datapath_loop/control.rs`), `virt/arcbox-vmm` (`darwin_hv` checkpoint); input to the host-side sandbox checkpoint design
- Outcome: open (spike). The checkpoint aborts every TCP flow in the datapath and saves the guest-bound resets. The restore sends them. A guest blocked in `recv` on a flow, which spike 4 left hanging, now gets ECONNRESET and reconnects 14–35 ms after the restore call
- Probes: `virt/arcbox-vmm/examples/checkpoint_linux_spike/` with `SPIKE_NET=1`, driven by `tests/bench/hv-checkpoint/run-net.sh`
- Follows: [the net and vsock spike](2026-10-31-hv-checkpoint-net-vsock-spike.md)
- Host / guest: as in spike 4. Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5
  (25F71), release build, the ArcBox kernel from boot assets 0.8.12 on
  1–8 vCPUs with 1 GiB RAM.

## Question

Spike 4 resets a stale flow only when the guest sends on it (finding 4
there). A guest that waits on a connection whose last segment the host
already ACKed sends nothing, so it keeps the dead connection until a
keepalive or its own timeout. Can the checkpoint record every flow, so
the restore can reset each one itself, and is the reset's sequence number
one the guest accepts?

## Hypotheses

1. `TcpBridge::abort` already builds the right frames: an RST at SND.NXT
   for an established flow, and the RFC 9293 replies for handshakes. Run
   after the I/O workers stop, SND.NXT is exactly what the guest has in
   RAM, which is its RCV.NXT once it resumes.
2. A Linux guest accepts those resets directly. It does not depend on
   spike 4's reactive RST, which answers the challenge ACK that Linux
   (RFC 5961) sends for a reset that is in the window but not exact.

## Method

- `NetworkDatapath::control` returns a `DatapathControl`, a channel into
  the datapath's event loop that is safe to use from other threads:
  - `abort_tcp` runs `TcpBridge::abort` in the loop. It returns the
    resets instead of sending them, and releases the flows' host sockets.
  - `send_to_guest` queues frames for the guest as reliable traffic.
- `HvMachine::checkpoint` calls `abort_tcp` after the I/O workers have
  stopped, and saves the frames in `state.json` (`tcp_resets`).
- `finish_restore` sends them once every vCPU has resumed.
- The checkpointed machine is stopped next anyway, so aborting its flows
  costs nothing. A checkpoint that lets the source continue would have to
  collect the same frames without closing the flows.

The spike 4 guest gains two TCP clients, each reconnecting when its
connection ends:

- *idle*: connects to a host server that never sends, and blocks in
  `recv` with no timeout.
- *download*: reads the host's endless u64 counter stream (about 1 GB/s
  here) and checks every word. The checkpoint therefore lands with
  download data in flight.

A restore passes when spike 4's checks hold and:
- Every TCP connection ends by ECONNRESET, and the vsock connection ends by
  ECONNRESET or ENOTCONN (below).
- The idle and download connections end exactly once and reconnect.
- The download makes progress after the restore.

Regression test: `abort_tcp_returns_resets_at_snd_nxt_and_send_to_guest_delivers_them`
(`datapath_loop/tests/control.rs`). It opens a flow and has the host send
24 bytes. It then checks that the returned reset is RST-only at ISN + 1 +
24 and that `send_to_guest` delivers it byte for byte.

## Results

Baseline, 4 vCPUs. The checkpoint aborted 3 flows (echo, idle, download)
and took 345 ms in total. The probe does not time the abort separately;
it falls inside the 9.5 ms step between stopping the I/O workers and
cloning the disk. After the
restore all three TCP clients got ECONNRESET, and all four servers had a
new connection 14–16 ms after the restore call. Over the next 5 s the
download moved 5.4 GB with no bad word.

20 save/restore cycles at random points: 20 of 20 passed, each with 3
flows aborted.

| Measure | min | p50 | max |
|---|---|---|---|
| `hv_restore`, ms | 2.9 | 3.1 | 4.0 |
| idle reconnect after the restore call, ms | 14 | 15 | 35 |
| download reconnect, ms | 14 | 15 | 35 |
| echo reconnect, ms | 14 | 15 | 35 |
| first vsock echo, ms | 13 | 15 | 35 |
| stopping the vCPUs, ms | 1.0 | 1.5 | 1.5 |
| stopping the I/O workers, ms | 0.7 | 5.8 | 11.7 |

vCPU sweep: 1, 2 and 8 vCPUs passed. Spike 3's block-only mode passed.

Negative controls, each a temporary edit and rebuild:

| Disabled | Result |
|---|---|
| sending the saved resets (`finish_restore`) | the idle connection does not end within 8 s (2 of 2). Echo and download still end: the resumed guest sends on them (the download ACKs the data already in its ring) and spike 4's reactive RST answers. This is the gap this spike set out to close |
| spike 4's reactive RST (`drain_tcp` change reverted), saved resets sent | all three TCP flows end by ECONNRESET: 3 of 3 restores of one checkpoint and 6 of 6 fresh save/restore cycles, the download included. One of those six restores failed a check because its vsock connection ended with ENOTCONN, not ECONNRESET; see finding 3 |

## Findings

1. **Hypothesis 1 holds.** No new flow-table format is needed. The
   frames `TcpBridge::abort` builds are the flow table in the only form the
   restore needs.
2. **Hypothesis 2 holds.** With the reactive RST disabled, a reset that
   was in window but not exact would draw a challenge ACK that nothing
   answers, and its flow would hang. None did, in 9 restores, with the
   download carrying about 1 GB/s at the checkpoint. SND.NXT read after the
   I/O workers stop is exact. The reactive RST stays as the fallback for
   flows the table cannot cover.
3. **A vsock connection can also end with ENOTCONN.** When the checkpoint
   lands between the guest echo server's read and its write, the write
   after TRANSPORT_RESET reports ENOTCONN. That is still a reset. The probe
   accepts it from 2026-10-31, and the spike 4 runs did not hit it.

## Not covered

Follow-up 2026-11-01: pending dials and ActiveOpen are covered by
[the gaps entry](2026-11-01-hv-checkpoint-gaps.md).

- Flows still waiting on a dial (`PassiveOpen` before the SYN-ACK) or a
  guest SYN-ACK (`ActiveOpen`, inbound port forwards). `abort` covers both,
  but the probe never checkpoints in that window. An ActiveOpen gets no
  reset by design (RFC 9293 3.10.5). Its guest should retransmit the
  SYN-ACK and get spike 4's reactive RST; this is not tested.
- UDP has no flow to reset. DHCP and DNS registrations remain as listed in
  spike 4.
