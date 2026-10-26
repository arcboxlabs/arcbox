# Borrowed TCP packet views

`TcpPacketView` validates the TCP protocol and header length after `IpPacketView` has parsed the IP packet. Fragments, including IPv6 atomic fragments, cannot bypass reassembly. The view borrows the complete TCP segment and exposes its transport fields and payload. Checksums remain separate because VM packets can carry offload checksums.

The packet tests use independent Scapy 2.6.1 IPv6 and smoltcp 0.13.1 IPv4 vectors. The vectors pin transport fields and payload offsets, including IPv6 extension chains and link padding. Additional cases reject incomplete TCP headers, invalid data offsets, non-TCP IP packets, and fragments.

All 53 packet tests passed on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 for `aarch64-apple-darwin`. This includes four new TCP regressions. Formatting, strict Clippy, and all-target checks for splicetcp and arcbox-proxy passed.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

Original App, NE, VPN, DNS, and interface snapshots were unchanged. These in-memory packet checks do not establish native lifecycle, TUN, or VM acceptance.
