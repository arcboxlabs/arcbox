# Pure TCP reset frames

`build_tcp_rst_only_frame` adds the reset form needed when the peer's ACK supplies the reset sequence number. The builder preserves the caller's sequence number and clears the acknowledgment, ACK flag, and receive window. The existing RST/ACK builder retains its behavior. Both builders support IPv4 and IPv6 through the shared encoder.

All 60 packet tests passed on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0. A new regression covers both address families with sequence numbers zero and `u32::MAX`, nonzero input acknowledgment and window fields, exact reset flags, and independent checksum sums. Formatting, strict Clippy, and the all-target splicetcp/proxy consumer check passed in a separate build directory for this source snapshot.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

The shared encoder is unchanged from the SYN batch. This API adds no performance claim. Original App, NE, VPN, DNS, and interface state remained unchanged; native lifecycle acceptance remains pending.
