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
  Push sources may pass bare IPv4/IPv6 packets to `classify_ip_packet` to avoid a framing copy.
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
async `oneshot::Receiver<EgressOutcome>`. `FlowMeta` contains the 4-tuple
and, when a shared fake-IP DNS log is installed on the bridge, the recovered
destination domain.

Consumers typically wrap their router/outbound layer behind this trait:

1. classify a SYN,
2. map `FlowMeta` to routing metadata,
3. dial an outbound,
4. return `EgressOutcome::Connect(EgressConn::Tcp(stream.into()))` for a real fd that
   the bridge can splice.

Return `EgressOutcome::Reject` to reset the guest handshake. Return
`EgressOutcome::Drop` to remove the handshake without a response. A closed
result channel also resets the guest. A `Drop` already delivered to the bridge
receiver stays silent even if the handshake deadline has elapsed before polling.
Pending results retain the handshake timeout behavior. Dropped handshakes retain
no transport or receiver; a later SYN for the same flow starts resolution again.

For event-driven loops, wrap the resolver and notify the loop when the inner
receiver completes. The wake should be sent **after** forwarding the result so
the next `poll_handshakes` observes the completed dial immediately. The
bridge's optional handshake waker cancels its forwarding task when the
handshake removes the result receiver.

## Abandoning an inspected connection

`EgressConn::controlled_tcp(stream)` returns a transport and an `AbandonHandle`.
The stream may be an unmanaged Tokio socket or a `NativeTcp` owner. Both address families preserve the owner through handshake and promotion.
The consumer must call `handle.abandon()` before dropping its loopback stream on
a late silent rejection. Dropping the handle alone preserves ordinary close
behavior. Controlled connections stay on the polled path, including when a
`ConnSink` is installed. The caller must continue calling `poll_fast_path`.

Abandonment releases the host socket and relay buffers. The bridge emits no
application data, FIN, or RST for that transition, including when abandonment
races with host EOF, an I/O error, or native socket cancellation. An abandonment signal also keeps a pending handshake silent during polling and final ACK processing. It retains cumulative TCP receive state,
discards payload, ACKs contiguous bytes, client FIN, and TCP keepalive probes,
and ignores other pure ACKs.
Out-of-order payload receives duplicate ACKs and requires retransmission.

The bridge removes discard state on client RST, after 120 seconds without a
client packet, or when admitting a new entry would exceed 4096 entries. Capacity
eviction removes the least recently active entry. Cleanup emits no packet.
These limits only bound retained state; they do not reproduce the connection
lifetime of any other TCP implementation. `fast_path_count()` counts host
transports only. Idle cleanup runs in `poll_fast_path` even when that count is
zero.

## Fast path and Tokio integration

Without a `ConnSink`, host→guest bytes are drained by
`TcpBridge::poll_fast_path`. This is simple and works in synchronous loops, but
the caller must keep polling while fast-path connections are active.

Each poll reads at most once per connection, up to 128 KiB within the guest's
available receive window. The bridge segments these bytes by the peer MSS and
configured MTU. Large-frame mode retains a 32 KiB read limit so each read fits
one IPv4 frame when the peer accepts GSO segments. IPv6 downloads always use normal segmentation with 20 fewer payload bytes than the IPv4 link budget and the peer MSS as an upper bound.

`TcpBridge::set_fast_path_mtu` also sets the MSS advertised in new guest-facing
handshakes. An MTU of 4000 advertises MSS 3960 for IPv4 and 3940 for IPv6, so uploads can use the link's full
packet budget. Downloads remain bounded by the peer's advertised MSS. Without
an explicit MTU, the bridge advertises MSS 1460 for IPv4 and 1440 for IPv6. Handshake retransmissions reuse
the original options, even if the configured MTU changes.

`FrameClassifier::drain_tcp` preserves arrival order across handshake completion
and subsequent data. A passive-open completion ACK enters the fast path after
promotion, including any payload and the advertised receive window. The bridge
forwards these bytes once and acknowledges only bytes accepted by the host socket.
Promotion requires explicit `FastPathInit` state. The bridge initializes the
guest receive window before publishing the stream to a `ConnSink`. A passive-open
completion ACK uses the negotiated window scale; an active-open SYN-ACK window
remains unscaled. If the peer omits Window Scale, later updates remain unscaled.
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
- `PromotedConn` and inline sinks carry IPv4 endpoints. IPv6 flows remain on the polled path.
- The Tokio sink emits segmented, standard-MTU-style TCP data frames and does
  not support `TcpBridge::enable_large_frames()`.
- If the sink rejects a connection, `TcpBridge` keeps that connection on the
  fallback polling path.

The first connection that requires polling allocates one shared 128 KiB read
buffer. The bridge reuses this buffer across connections and keeps it until
the bridge is dropped. A bridge whose connections are all owned by a `ConnSink`
does not allocate this buffer.

The internal SYN rejection encoder validates IP/TCP structure and matching Ethernet/IP families. Its RST|ACK response swaps endpoints, excludes padding, and acknowledges the SYN plus its declared payload. The classifier, rejection encoder, and handshake parser support both address families.

## Egress addresses

`FlowMeta` and `FlowKey` use `IpAddr` for both endpoints. Existing IPv4 callers convert addresses with `.into()`. `DefaultEgress` dials IPv4 and IPv6 destinations, retains recovered domains for proxies, and maps the configured IPv4 gateway to IPv4 loopback. Proxy precedence and bypass rules apply to both families. Stored TCP bridge endpoints also use `IpAddr`; promoted IPv6 flows produce IPv6 data and control frames. Established Ethernet and bare-IP input accepts IPv4 and IPv6 through shared IP/TCP views. Declared lengths exclude padding; truncated headers, incomplete fragments, and mismatched Ethernet/IP families are rejected before flow state changes. IPv4 and IPv6 uploads share bounded vectored writes. Coalescing compares complete endpoints, contiguous sequence numbers, and identical ACK/window state after shared IP/TCP validation. Active and passive handshake parsing accepts both families through the same shared views. Without a peer MSS option, IPv6 uses 1220; IPv4 retains 536 for passive opens and 1460 for active opens. Classifier entry points accept both families.

## Managed native sockets

`NativeTcp` preserves an opaque owner lease across Tokio/std conversion and
descriptor cloning. Cancellation observers retain only a token. Each native
socket closes before its lease is released; token cancellation alone does not
mean that every descriptor has closed.

Consumers that cannot retain the lease must call `into_unmanaged` and reject a
managed socket.

`EgressConn::Tcp` carries `NativeTcp<tokio::net::TcpStream>`. Resolvers wrap an unmanaged socket with `.into()` or attach a lease with `NativeTcp::with_lifecycle`. The bridge retains that owner through handshake, promotion, uploads, and cloned readers. Inbound and promotion entry points accept either wrapper or an unmanaged socket. The Tokio frame sink retains the wrapper; the VMM inline sink rejects managed sockets and leaves the primary socket on the polled path.

Each accepted reader has a child cancellation token. Replacing or dropping the primary cancels that reader without cancelling other connections in the same generation. Tokio cancellation interrupts socket reads, zero-window waits, and blocked frame sends. A clean reader EOF preserves the primary upload half. Fast-path polling and packet interception reset cancelled flows before socket I/O. Peer resets close the flow without a reply. Handshake polling and completion reject cancelled sockets. Abandoning a handshake also drops its dial-result forwarding receiver. Active-open cancellation sends a sequence-correct RST only after sending the SYN.

## L3 tunnel shim

`FrameClassifier` and `TcpBridge` operate on Ethernet frames. Host tunnel links
such as macOS `utun` or iOS `NEPacketTunnelFlow` carry bare IP packets instead.
The `shim` module provides the adapter:

- `L3ToL2Source` wraps an L3 `FrameSource` and prepends a fixed synthetic
  Ethernet header before classification.
- `FrameClassifier::classify_ip_packet` accepts bare IPv4/IPv6 packets from push
  sources. Retained packets receive the synthetic Ethernet header directly in
  their owned buffer. Both families use the same reassembler as Ethernet input.
- `TcpBridge::try_fast_path_intercept_ip_batch` borrows established IPv4 or IPv6 TCP packets
  for synchronous socket writes, avoiding the classifier's owned-frame copy.
  Callers must drain preceding classifier input before interception to preserve
  handshake and fragment order. SYNs and fragments still use the classifier.
  Backpressure and out-of-order segments retain owned payload copies.
- `synthetic_l2_header` plus `l3_to_l2` remain available for consumers that need
  a contiguous Ethernet frame before classification.
- `l2_to_l3` strips the synthetic Ethernet header from guest-bound frames and
  drops ARP or non-IP frames that have no L3 representation.

The L3 adapter accepts IPv4 and ordinary IPv6 packets, validates IP structure, trims padding, and selects the EtherType per packet. Reusable read and framing buffers hold the maximum IPv6 packet (40 + 65535 bytes). IPv6 jumbograms and unsupported extensions are rejected. TCP and UDP classification uses the IPv4/IPv6 reassembler. ICMPv6 is not handled. Callers must supply IPv6 UDP egress; ArcBox HostEgress handles IPv4 and rejects other EtherTypes before parsing.

## Memory and performance notes

- `FrameClassifier::new` uses a large packet pool suitable for the VM datapath.
  Memory-constrained consumers should use `with_pool_capacity`.
- `TcpBridge::enable_large_frames()` disables MSS segmentation for callers whose
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

The fragment reassembler supports bounded IPv4 and IPv6 tables, each with 32 contexts and 64 disjoint ranges per datagram. IPv6 contexts expire after 3 seconds. Conflicting overlaps quarantine an ID until expiry; exact duplicates add no coverage. Atomic fragments are isolated from contexts with the same ID. Reassembly validates extension chains, first transport headers, final lengths, and ECN combinations before returning a complete frame.

The default egress resolver accepts IPv6 system-proxy endpoint addresses. HTTP, HTTPS, and SOCKS5 proxy selection shares the authority formatting from `ProxyConfig`; destination routing and proxy precedence are unchanged.

The internal `tcp_bridge::segmentation` module shares download and retransmission frame construction. The `tcp_bridge_bench` example measures established ACK handling and polled downloads through public bridge APIs with 1 or 64 active flows.

The `tcp_upload_bench` example compares single-packet and 32-packet uploads through Ethernet and bare-IP APIs for both families. Timing includes encoding, socket writes, ACK parsing, peer reads, and payload validation; connection setup is excluded.

`TcpSynInfo` stores both endpoints as `IpAddr`. Classifier entry points accept both families. Shared IP/TCP validation now excludes padding and rejects truncated transport headers before retaining frames; UDP declared lengths must fit the IP payload. The `classifier_bench` example measures classification, retained-frame copies, queue draining, and byte validation through both Ethernet and bare-IP APIs.

## TCP CLOSED replies

After dropping the bridge and upstream resources, callers may pass complete IP packets to `TcpBridge::closed_reply_ip` during a bounded control-only receive phase. This stateless method implements RFC 9293 CLOSED processing: discard RST input, reply to ACK input with `SEQ=SEG.ACK` and RST only, and reply to other TCP input with RST|ACK using `ACK=SEG.SEQ+SEG.LEN`. Segment length includes payload, SYN, and FIN.

The result is an Ethernet frame with the supplied MAC addresses. No flow or upstream resource is retained. Malformed packets, fragments, non-TCP packets, unspecified addresses, multicast addresses, and IPv4 limited-broadcast addresses produce no reply. Callers must exclude subnet-directed broadcasts using their interface configuration. Offload checksums are not validated on input.

## Explicit TCP abort

Stop business ingress and discard queued business packets before calling `TcpBridge::abort`. Keep guest output available until the reset callback completes. The bridge drains all flows without forwarding buffered bytes. Each flow releases its primary socket and cancels its reader before the callback. Retained sink owners may keep descriptors until cancellation completes. Established flow byte totals use the same observer path as normal close, once per removed flow.

RFC 9293 section 3.10.5 defines explicit ABORT separately from connection cancellation. Active-open handshakes are deleted without a reset. Passive handshakes with a generated SYN-ACK and established flows send RST without ACK at `SND.NXT`. Pending passive dials send RST|ACK acknowledging the original SYN, payload, and FIN. No final FIN is synthesized. A second abort emits nothing unless new flows exist. Use `closed_reply_ip` for subsequent guest packets during a bounded control phase.
