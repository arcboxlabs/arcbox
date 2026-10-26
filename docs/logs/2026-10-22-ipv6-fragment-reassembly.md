# Bounded IPv6 fragment reassembly

The internal reassembler now handles IPv6 Fragment headers before transport parsing. It preserves extension chains, isolates atomic fragments, quarantines conflicting overlaps until expiry, and bounds context count, received ranges, and payload length. IPv4 behavior is preserved. Classifier and shim integration remain separate work.

All 135 splicetcp library tests passed with all features on the dedicated `xuan-work-mbp` test Mac using Rust 1.96.0. Six new IPv6 tests cover both the parent dispatcher and the implementation, out-of-order delivery, exact duplicates, overlap quarantine, atomic isolation, invalid final lengths, oversize, nested fragments, incomplete first transport headers, context/range limits, expiry, and ECN. Formatting, strict all-target Clippy, and consumer checks passed with a separate build directory.

Reproduce with `devenv shell -- cargo test --locked -p splicetcp --lib --all-features`, `devenv shell -- cargo clippy --locked -p splicetcp --all-targets --all-features -- -D warnings`, and `devenv shell -- cargo fmt --all --check`. Original App, NE, VPN, DNS, and interface state remained unchanged. These are userspace checks; native TUN/application acceptance and performance measurements remain pending.

The supplied Surge analysis describes address-family dispatch in `surge-rea-analysis.md:56` and QUIC datagram reassembly in `surge-rea-protocols.md:85–95`. The latter is a separate protocol layer; its limits do not define IPv6 IP reassembly behavior or prove interoperability.
