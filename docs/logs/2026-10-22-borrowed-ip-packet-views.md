# Borrowed IP packet views

`arcbox-packet::ip` provides a common IPv4/IPv6 parsing boundary for native and Ethernet consumers. The view trims link padding, walks supported IPv6 extensions, and separates fragment metadata from transport bytes. Active source routing, late Hop-by-Hop headers, malformed options, and jumbograms are rejected. Fragment reassembly and checksum verification remain separate responsibilities.

The supplied Surge analysis describes VIF dispatch by address family and IP protocol (`surge-rea-analysis.md`, line 56). Its QUIC fragmentation analysis concerns protocol datagrams, not IP fragment parsing (`surge-rea-protocols.md`, lines 85–95). These observations support keeping the parsing and reassembly layers separate; they are not interoperability evidence.

The dependency was added with `cargo add -p arcbox-packet etherparse@0.21`. Cargo resolved etherparse 0.21.0 and arrayvec 0.7.8 without upgrading existing packages.

Validation on the dedicated `xuan-work-mbp` test Mac used Rust 1.96.0 for `aarch64-apple-darwin`. All 49 packet tests passed, including five new regressions for declared lengths, padding, addresses, fragments, and IPv6 extension restrictions. Formatting, strict packet Clippy, and all-target consumer checks passed.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

The first result collector omitted Rust's `- should panic` suffix even though all tests passed. The corrected collector matched all discovered tests exactly. Original App, NE, VPN, DNS, and interface snapshots were unchanged. This batch does not establish native lifecycle, TUN, or VM acceptance.
