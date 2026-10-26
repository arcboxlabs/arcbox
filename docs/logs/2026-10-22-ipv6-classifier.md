# IPv6 classifier activation

Ethernet and bare-IP classifier entry points now accept IPv4 and IPv6 TCP/UDP packets. Ethernet input requires matching EtherType and IP family. Bare input receives the corresponding synthetic EtherType and rejects input exceeding the ordinary family limit. IPv6 fragments reach the existing bounded reassembler before TCP/UDP dispatch. Maximum IPv6 packets use the existing heap fallback when the packet pool cannot hold them.

The dedicated xuan-work-mbp test Mac passed 152 splicetcp tests and 34 arcbox-proxy tests, with all features and no ignored tests. Strict all-target/all-feature Clippy, workspace formatting, and all-target compilation for splicetcp, arcbox-proxy, and arcbox-net passed. The socket regression covers both input forms, a wrapping SYN sequence, out-of-order fragments carrying the final handshake ACK and payload, deferred promotion, duplicate input, and exactly-once payload delivery through EOF. UDP regressions cover ports 53/67/443, maximum length, heap fallback, and oversized input. Existing malformed-input coverage now checks every truncated IPv6/TCP prefix and mismatched Ethernet families.

IPv6 UDP is retained for the caller's UDP handler. Gateway DNS, DHCP, and ICMP interception remain IPv4-specific. ArcBox HostEgress still uses IPv4 offsets, so its entry now rejects other EtherTypes before inbound relay or transport parsing. A regression embeds the IPv4 UDP protocol value in an IPv6 source address and verifies that no IPv4 flow or socket task is created. IPv6 UDP forwarding and ICMPv6 remain separate consumer work.

The user-supplied `surge-rea-analysis.md`, section on utun reads, describes AF_INET/AF_INET6 selection before TCP/UDP/ICMP dispatch. This classifier follows that family-before-transport boundary. The report is static analysis, not interoperability evidence. Surge's ICMPv6 dispatch described there remains an explicit gap.

Paired timing is recorded in [network benchmarks](../benchmarks/network.md#2026-10-22--ipv6-classifier). App, NE, VPN, DNS, and interfaces remained unchanged. No native tunnel or VM acceptance is claimed.
