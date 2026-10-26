# IPv4 and IPv6 TCP SYN frames

SYN and SYN/ACK now use the shared frame encoder with TCP options. `SynParams<A>` and `SynAckParams<A>` retain IPv4 as the default address type and also accept IPv6. Mixed address families are rejected. Pure SYN frames have acknowledgment zero; both builders advertise a receive window of 65535.

All 59 packet tests passed on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 for `aarch64-apple-darwin`. The new table covers both IP families, both SYN flags, and all WScale/SACK presence combinations, including scale shifts 0 and 14. It checks exact option bytes, NOP padding, lengths, ports, sequence numbers, acknowledgments, windows, and independently computed checksums. Existing IPv4 fixtures and consumer checks also passed.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

The [dated benchmark](../benchmarks/network.md#2026-10-22--tcp-options-in-the-shared-frame-encoder) measures the existing IPv4 data path after adding options to the shared encoder. Original App, NE, VPN, DNS, and interface state remained unchanged. Native TUN and application acceptance remain separate checks.
