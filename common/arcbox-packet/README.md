# arcbox-packet

Pure packet parsing, Ethernet framing, and checksum primitives for native and VM datapaths. The crate has no I/O or virtualization dependency.

`IpPacketView` borrows an IPv4 or IPv6 packet and trims padding to the declared IP length. It validates extension lengths, option discard actions, and supported routing behavior. Jumbograms are unsupported. Checksums remain the caller's responsibility because VM packets can contain offload checksums.

```rust
use arcbox_packet::ip::IpPacketView;

fn addresses(packet: &[u8]) -> Option<(std::net::IpAddr, std::net::IpAddr)> {
    let ip = IpPacketView::parse(packet)?;
    Some((ip.source(), ip.destination()))
}
```

`transport()` returns `None` for fragments, including IPv6 atomic fragments. Use `fragment()` to obtain the identification, byte offset, prefix, and payload for reassembly. The reassembler must validate fragment alignment, overlaps, and size limits before transport parsing.

`TcpPacketView::parse(ip)` accepts TCP only after IP parsing and fragment reassembly. The TCP view exposes addresses, ports, sequence numbers, control flags, the receive window, and payload without copying packet bytes. It rejects incomplete TCP headers and invalid data offsets.

TCP ACK, data, FIN/ACK, and RST/ACK builders accept IPv4 or IPv6 addresses through `TcpFrameParams<A>`. Both addresses must have the same IP family; IPv4 remains the default type. Data frames accept at most 65495 payload bytes for IPv4 or 65515 for IPv6. ACK and data frames use the requested receive window. FIN/ACK frames use 65535; RST/ACK frames use zero. All four builders compute complete TCP checksums and the IPv4 header checksum. IPv6 uses its own pseudo-header checksum. Partial-checksum GSO builders accept IPv4 only.

SYN and SYN/ACK builders accept IPv4 or IPv6 through `SynParams<A>` and `SynAckParams<A>`. They always include the supplied MSS, include WScale and SACK-Permitted when requested, and pad options with NOP bytes. Both use a receive window of 65535; pure SYN frames use acknowledgment zero.

`build_tcp_rst_only_frame` constructs a reset with the ACK flag clear, acknowledgment zero, and receive window zero. It preserves the supplied sequence number and supports both IP families. Use `build_tcp_rst_frame` when the reset must acknowledge a segment.
