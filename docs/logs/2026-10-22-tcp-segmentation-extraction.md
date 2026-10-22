# TCP segmentation extraction

Move the existing IPv4 download and retransmission encoder into the internal `tcp_bridge::segmentation` module. Frame construction, sequence accounting, MSS limits, and partial-checksum behavior remain unchanged. This boundary prepares the bridge to carry IPv6 addresses without expanding the interception module.

The dedicated xuan-work-mbp test Mac passed 144 splicetcp library tests with all features and no ignored tests. Strict all-target/all-feature Clippy, workspace formatting, and default-feature consumer compilation also passed with Rust 1.96.0 and a fresh build directory. The first snapshot failed Clippy because the extraction left an unused import; the corrected snapshot passed all checks.

The new public-API benchmark establishes real bridge flows over owned loopback sockets. Its download loop handles partial socket reads and checks content, sequence numbers, and ACK acceptance. The [benchmark results](../benchmarks/network.md#2026-10-22--tcp-segmentation-extraction) compare independently built snapshots. Original App, NE, VPN, DNS, and interface state remained unchanged. Native lifecycle and end-to-end IPv6 acceptance remain pending.
