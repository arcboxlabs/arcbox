# IPv6 L3 adapter and utun capacity

The L3 adapter now wraps IPv4 and IPv6 packets with the matching EtherType and retains the reusable synthetic MAC header. Shared IP parsing trims padding and rejects truncated or unsupported packets. The read and framing buffers include the 40-byte IPv6 header in addition to the maximum 16-bit payload. The L2 egress filter accepts both IP EtherTypes.

The dedicated xuan-work-mbp test Mac passed 140 splicetcp library tests with all features and no ignored tests, plus strict all-target/all-feature Clippy, workspace formatting, and default-feature consumer compilation. Validation used Rust 1.96.0 and a fresh build directory. New tests cover family changes using one header, padding, exact and undersized buffers, truncated declarations, maximum IPv6 read/framing capacity, and IPv6 output with a network-order AF header. Unix datagram socket pairs test the readv/writev boundary without creating a utun device.

The [L3 benchmark](../benchmarks/network.md#2026-10-22--l3-adapter-validation) records the parser cost. The Surge analysis supplied by the user separates address-family dispatch from protocol dispatch; this batch establishes the framing boundary only. Classifier, TCP bridge, native lifecycle, and protocol interoperability acceptance remain pending. Original App, NE, VPN, DNS, and interface state remained unchanged.
