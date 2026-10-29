# splicetcp

`splicetcp` is ArcBox's userspace TCP-termination and splice bridge. It sits on
the frame plane between an L2/L3 packet source and an egress dialer: it
classifies Ethernet frames, synthesizes the guest-facing TCP handshake, and then
promotes established flows to a fast path backed by a real host `TcpStream`.

It is intentionally **not** a full TCP/IP stack. The guest kernel still owns the
guest TCP state. `splicetcp` only gates outbound SYNs, creates the host-side
egress connection, mirrors the minimal sequence/ack state needed for frame
construction, and splices bytes between the guest frames and the host socket.

Both guest-facing handshake directions omit SACK-Permitted. The fast path
uses cumulative ACKs and does not generate or consume SACK blocks. Keep SACK
disabled until the fast path implements selective acknowledgment; peer loss
recovery can otherwise wait for retransmission timers instead of duplicate ACKs.

## Architecture

```text
packet source
  │
  ▼
FrameClassifier ── ARP / DHCP / DNS / UDP / ICMP ──▶ caller-owned handlers
  │
  ├─ gated TCP SYN ───────────────▶ TcpBridge::handle_outbound_syn
  │                                  │
  │                                  ▼
  │                                EgressResolver::resolve
  │                                  │
  │                                  ▼
  │                                EgressConn::Tcp
  │                                  │
  ▼                                  ▼
TCP data/ACK frames ◀──────────── TcpBridge fast path
```

The crate is split around a few seams:

- `classifier::FrameClassifier` is a pure Ethernet-frame demultiplexer. It owns
  no fd; callers feed frames via `classify_frame` and drain the resulting queues.
  Push sources may pass bare IPv4 or IPv6 packets to `classify_ip_packet` to avoid a framing copy.
- `tcp_bridge::TcpBridge` performs SYN gating, handshake synthesis, and the
  promoted fast path.
- `egress::EgressResolver` is the policy/dial seam. The default resolver dials
  directly or through the system proxy; product code can inject its own router
  and outbound stack.
- `direct_rx::{ConnSink, PromotedConn}` lets callers take ownership of
  promoted host→guest reads instead of polling `TcpBridge::poll_fast_path`.
- `shim` adapts bare L3 links (`utun`, Network Extension packet flows) to the
  L2 classifier by adding/removing a synthetic Ethernet header.
- `utun` provides macOS helpers for the 4-byte AF header used by `utun` fds.

## Egress resolution

`TcpBridge` calls `EgressResolver::resolve` for every gated SYN and expects an
async `oneshot::Receiver<Option<EgressConn>>`. `FlowMeta` contains the 4-tuple
and, when a shared fake-IP DNS log is installed on the bridge, the recovered
destination domain.

Consumers typically wrap their router/outbound layer behind this trait:

1. classify a SYN,
2. map `FlowMeta` to routing metadata,
3. dial an outbound,
4. wrap an unmanaged socket with `NativeTcp::new(stream)`, or attach its opaque
   owner lease and cancellation token with `NativeTcp::with_lifecycle`,
5. return `Some(EgressConn::Tcp(stream))` for the wrapped native socket.

For event-driven loops, wrap the resolver and notify the loop when the inner
receiver completes. The wake should be sent **after** forwarding the result so
the next `poll_handshakes` observes the completed dial immediately.

## Fast path and Tokio integration

Without a `ConnSink`, host→guest bytes are drained by
`TcpBridge::poll_fast_path`. This is simple and works in synchronous loops, but
the caller must keep polling while fast-path connections are active.

Each poll reads at most once per connection, up to 128 KiB within the guest's
available receive window. The bridge segments these bytes by the peer MSS and
configured MTU. Large-frame mode retains a 32 KiB read limit so each read fits
one IPv4 frame.

`TcpBridge::set_fast_path_mtu` also sets the MSS advertised in new guest-facing
handshakes. An MTU of 4000 advertises MSS 3960 for IPv4 and 3940 for IPv6, so uploads
can use the link's full packet budget. Downloads remain bounded by the peer's advertised MSS. Without
an explicit MTU, the bridge advertises MSS 1460 for IPv4 and 1440 for IPv6. Handshake retransmissions reuse
the original options, even if the configured MTU changes.

`FrameClassifier::drain_tcp` preserves arrival order across handshake completion
and subsequent data. A passive-open completion ACK enters the fast path after
promotion, including any payload and the advertised receive window. The bridge
forwards these bytes once and acknowledges only bytes accepted by the host socket.
Established uploads coalesce at most 64 consecutive frames
and 256 KiB into one vectored socket write and cumulative ACK. Coalescing requires
the same connection, contiguous sequence numbers, identical download ACK/window
state, no control flags other than ACK/PSH, and no retained upload data. Other
frames use the single-frame path. Partial writes retain every unwritten suffix;
only bytes accepted by the host socket advance the ACK.

`poll_fast_path` also retries retained guest→host bytes after host socket
backpressure, including for inline flows. Recovery batches contiguous retained
bytes within the same 64-segment and 256 KiB limits, skips retransmission overlap,
and stops at the first missing range. Upload ACKs advance only over bytes
accepted by the host socket. Hard write errors, including `BrokenPipe`, reset
the guest flow without ACKing rejected bytes. Pending upload data shares the
reassembly limits of 4 MiB and 4096 segments per flow. A contiguous tail takes
priority over future segments at those limits. Evicted bytes remain
unacknowledged and require guest retransmission. A missing prefix still
requires guest retransmission before parked bytes can drain.

Enable the `tokio-frame-sink` feature to use
`direct_rx::TokioFrameConnSink` instead:

```rust,no_run
use splicetcp::direct_rx::TokioFrameConnSink;
use splicetcp::tcp_bridge::TcpBridge;

# let gateway_ip = std::net::Ipv4Addr::new(10, 0, 2, 2);
let mut bridge = TcpBridge::new(gateway_ip);
let (sink, mut frames) = TokioFrameConnSink::channel(1024);
bridge.set_conn_sink(sink);

// Await `frames.recv()` in the datapath loop and write each Ethernet frame to
// the guest-facing sink.
```

`TokioFrameConnSink` turns each accepted `PromotedConn` into a Tokio read task
and emits guest-bound Ethernet frames through a bounded `mpsc` channel. Channel
backpressure pauses socket reads instead of allowing unbounded buffering.

Important constraints:

- `send_conn` must be called from within a Tokio runtime; accepted connections
  are driven by `tokio::spawn`.
- The Tokio sink accepts IPv4 flows and emits segmented TCP data frames. It does
  not support `TcpBridge::enable_large_frames()`.
- If the sink rejects a connection, `TcpBridge` keeps that connection on the
  fallback polling path.

The first connection that requires polling allocates one shared 128 KiB read
buffer. The bridge reuses this buffer across connections and keeps it until
the bridge is dropped. A bridge whose connections are all owned by a `ConnSink`
does not allocate this buffer.

## Managed native sockets

`NativeTcp` preserves an opaque owner lease across Tokio/std conversion and
descriptor cloning. Cancellation observers retain only a token. Each native
socket closes before its lease is released; token cancellation alone does not
mean that every descriptor has closed. A retained oneshot result, unpolled
bridge, or retained sink item can delay owner release.

Bridge polling and interception remove cancelled native flows with a reset.
The primary owns the promoted reader's cancellation guard, so replacing a flow
or dropping the bridge also stops that reader. Clean reader EOF preserves the
primary upload half. The Tokio sink selects cancellation around the entire
reader, including receive-window waits and blocked frame-channel sends.

Call `TcpBridge::abort` while guest output remains available to explicitly stop
TCP flows. Stop business ingress and discard queued business packets first. The bridge
drains handshake and established flows without polling business I/O or forwarding
buffered bytes. Each flow releases its primary socket and cancels its reader
before the callback receives an RST Ethernet frame. Deliver these frames before
removing the guest interface; no final FIN is synthesized. Existing sink owners
may retain their descriptors until cancellation completes.

An established reset uses `SND.NXT` as its sequence number and leaves the ACK
flag clear. The guest may not have received every generated frame, so one reset
cannot guarantee peer acceptance. After dropping the bridge, callers may pass
complete IP packets to `TcpBridge::closed_reply_ip`. This stateless method
implements RFC 9293 CLOSED processing: discard RST input, reply to ACK input with
`SEQ=SEG.ACK` and RST only, and reply to other TCP input with RST|ACK using
`ACK=SEG.SEQ+SEG.LEN`. Segment length includes payload, SYN, and FIN.
The reply is an Ethernet frame; the method retains no flows or upstream resources.
Malformed packets, fragments, and non-TCP packets produce no reply. The method
also rejects unspecified, multicast, and IPv4 limited-broadcast endpoints.
Callers must exclude subnet-directed broadcasts using their interface configuration.
Callers must bound this control-only receive phase before removing the guest interface.

Explicit abort drops active-open handshakes without a reset. Passive handshakes
whose SYN-ACK was generated use `SND.NXT` with RST only; pending passive SYNs
receive RST|ACK acknowledging the original SYN and its payload.
Explicit abort reports established-flow byte totals through the same
observer as ordinary close. A second abort emits nothing and reports no duplicate
close unless new flows have been admitted.

Consumers that cannot retain the lease must call `into_unmanaged` and reject a
managed socket. The VMM inline sink uses that rejection to keep managed flows
on the existing native polling path.

## L3 tunnel shim

`FrameClassifier` and `TcpBridge` operate on Ethernet frames. Host tunnel links
such as macOS `utun` or iOS `NEPacketTunnelFlow` carry bare IP packets instead.
The `shim` module provides the adapter:

- `L3ToL2Source` wraps an L3 `FrameSource` and prepends a fixed synthetic
  Ethernet header before classification.
- `FrameClassifier::classify_ip_packet` accepts bare IPv4 or IPv6 packets from push
  sources. Retained packets receive the synthetic Ethernet header directly in
  their owned buffer. IP fragments use the same reassemblers as Ethernet input.
- `TcpBridge::try_fast_path_intercept_ip_batch` borrows established TCP packets
  for synchronous socket writes, avoiding the classifier's owned-frame copy.
  Callers must drain preceding classifier input before interception to preserve
  handshake and fragment order. SYNs and fragments still use the classifier.
  Backpressure and out-of-order segments retain owned payload copies.
- `synthetic_l2_header` plus `l3_to_l2` remain available for consumers that need
  a contiguous Ethernet frame before classification.
- `l2_to_l3` strips the synthetic Ethernet header from guest-bound frames and
  drops ARP or non-IP frames that have no L3 representation.

The native entry points support ordinary IPv4 and IPv6 TCP/UDP packets. Shared
borrowed packet views validate declared lengths and traverse IPv6 extension
headers before TCP parsing. IPv6 jumbograms and active source-routing headers
are rejected. The shim holds a maximum 65,575-byte IPv6 packet plus Ethernet
framing. The native utun reader reserves the same IP capacity. Classifier frames
beyond the packet pool capacity use owned heap
storage without truncation. The VM file-descriptor source retains its existing
65,535-byte frame limit.

Each address family has 32 fragment contexts with a three-second expiry and 64
received-range limit per context. Across both families this bounds payload
storage to approximately 4 MiB. IPv6 reassembly removes the Fragment header,
retains the extension chain, and quarantines overlapping datagrams until expiry.
Atomic fragments do not alter a same-ID fragmented datagram.

IPv6 flows use the existing polling state machine for handshake, upload,
download, retransmission, half-close, and cancellation. The inline sink and
partial-checksum/GSO APIs remain IPv4-only. IPv6 segmentation accounts for the
40-byte base header and honors smaller peer MSS values; an absent peer MSS
uses 1220. Flow keys retain the on-wire address family, including mapped addresses.

## Memory and performance notes

- `FrameClassifier::new` uses a large packet pool suitable for the VM datapath.
  Memory-constrained consumers should use `with_pool_capacity`.
- `TcpBridge::enable_large_frames()` disables IPv4 MSS segmentation for callers whose
  guest-facing transport can carry large frames. Do not enable it on standard
  MTU `utun`/packet-tunnel paths unless the entire write path is sized for those
  frames.
- For idle event-driven loops, rely on explicit wakes: dial completion from an
  `EgressResolver` wrapper, UDP/DNS handler wakes from the consumer, and
  `TokioFrameConnSink` for host→guest TCP readiness. A long backstop timer is
  fine as a missed-wake guard, but should not be the steady-state driver.

## Feature flags

- `tokio-frame-sink`: exposes `direct_rx::TokioFrameConnSink`, the Tokio-backed
  `ConnSink` for event-driven promoted host→guest reads.
