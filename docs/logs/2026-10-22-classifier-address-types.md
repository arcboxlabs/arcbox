# Classifier address types

`TcpSynInfo` now stores `IpAddr` endpoints. The IPv4 classifier converts parsed IPv4 addresses at the construction boundary. Existing production consumers use the retained frame and require no behavior changes. IPv6 dispatch remains pending.

The dedicated xuan-work-mbp test Mac passed 149 splicetcp library tests with all features and no ignored tests, strict all-target/all-feature Clippy, workspace formatting, and all-target compilation for splicetcp, arcbox-proxy, and arcbox-net. The existing mixed-protocol regression still verifies the typed SYN endpoints and original frame bytes. App, NE, VPN, DNS, and interface state remained unchanged.

The new `classifier_bench` driver establishes the measured baseline for shared parsing and IPv6 activation. It checks retained bytes for SYN and data frames through both entry points. Results and reproducible commands are recorded in [network benchmarks](../benchmarks/network.md#2026-10-22--classifier-address-types).
