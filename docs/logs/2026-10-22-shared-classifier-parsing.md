# Shared classifier parsing

The classifier now parses each incoming IP packet once and passes the validated view to transport dispatch. TCP requires a complete declared header and segment. UDP requires its declared length to fit the IP payload. Retained frames exclude trailing padding. Fragment reassembly precedes transport dispatch and reparses the reconstructed packet. Entry points remain IPv4-only pending IPv6 activation.

The dedicated xuan-work-mbp test Mac passed 150 splicetcp library tests with all features and no ignored tests, strict all-target/all-feature Clippy, workspace formatting, and all-target compilation for splicetcp, arcbox-proxy, and arcbox-net. The new regression verifies both entry points, every truncated prefix, undersized or oversized IP/TCP/UDP lengths, unchanged queues for invalid input, and retained bytes without padding. The legacy minimal TCP fixture now contains its required data-offset field; the old classifier accepted a zero field without parsing the TCP header.

Paired classifier results are recorded in [network benchmarks](../benchmarks/network.md#2026-10-22--shared-classifier-parsing). App, NE, VPN, DNS, and interfaces remained unchanged. This batch does not establish native IPv6 acceptance.
