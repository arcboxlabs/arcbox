# 2026-11-01 — Large-frame retransmits with more than 64 KiB in flight panicked splicetcp

- Type: change
- Area: `common/splicetcp` (`tcp_bridge/segmentation.rs`)
- Commits / PRs: `fix(splicetcp): split large-frame retransmits into bounded frames`
- Related: [HV checkpoint gaps experiment](../experiments/2026-11-01-hv-checkpoint-gaps.md)

## Problem or trigger

One `tests/bench/hv-checkpoint/run-net.sh` restore crashed the datapath with
`build_tcp_data_frame_partial_csum: ip_total_len=… overflows u16`. The restore
only made the condition likely (a stalled guest, a deep in-flight backlog); the
bug is in the production download path.

In large-frame mode (`TcpBridge::enable_large_frames`, set whenever the
datapath has a channel frame sink) `emit_data_frames` built **one** GSO frame
from whatever slice it was given. First transmissions are bounded by the
32 KiB large-frame read cap, so they always fit. Both loss-recovery paths —
the polled flow's `retransmit_buf` and the inline flow's shared ring — hand it
every in-flight byte at once, up to `HONORED_WINDOW_CAP` (256 KiB). With more
than 65495 bytes in flight the IPv4 total-length assertion fires and the
datapath thread panics. Standard-MTU mode was never affected: it already
segments by MSS.

## What was done

`emit_data_frames` splits the large-frame branch into frames of at most
`LARGE_FRAME_MAX_PAYLOAD` (32 KiB), the same constant the first-transmission
read cap now uses. It is the only splicetcp caller of
`build_tcp_data_frame_partial_csum`; the other data-frame builders
(`direct_rx`, the persist probe) already pass bounded buffers. The builder's
existing assertion stays as the backstop.

## Evidence

`tcp_bridge/tests/retransmit_frames.rs` puts 160 KiB in flight on a
large-frame flow (window scale 2) and triggers each path: polled RTO, polled
triple dup-ACK, inline RTO, inline triple dup-ACK. It asserts every frame's
payload is 1..=32 KiB, the IP length matches the frame, and the frames re-send
the in-flight range contiguously and completely.

- Without the fix: all four panic with
  `ip_total_len=163880 overflows u16`, the production signature.
- With the fix: all four pass; each retransmission is five 32 KiB frames.

## Follow-ups

None.
