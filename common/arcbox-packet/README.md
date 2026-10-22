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
