# Egress address families

`FlowMeta` and the public observer `FlowKey` now preserve IPv4 or IPv6 addresses through `IpAddr`. `DefaultEgress` uses `SocketAddr` for direct dialing, so an IPv6 destination no longer requires an IPv4-only target constructor. The configured IPv4 gateway still maps to IPv4 loopback and bypasses system proxies. Existing bridge callers convert their IPv4 addresses at the egress boundary.

The dedicated xuan-work-mbp test Mac passed all 142 splicetcp library tests with all features, no ignored tests, strict all-target/all-feature Clippy, workspace formatting, and default-feature splicetcp/arcbox-proxy compilation. Validation used Rust 1.96.0 and a fresh build directory. The new socket test exchanges request and response bytes over IPv4 loopback, IPv6 loopback, and the IPv4 gateway mapping. An owned proxy listener confirms that gateway traffic does not reach the configured proxy. Selection tests retain IPv6 literals, recovered domains, proxy precedence, and bypass rules.

This batch changes connection setup and flow-close metadata. Packet parsing, data transfer loops, native socket ownership, and classifier dispatch remain unchanged. Native lifecycle and complete IPv6 protocol acceptance remain pending. Original App, NE, VPN, DNS, and interface state remained unchanged.
