# IPv6 TCP control and data frames

TCP ACK, data, FIN/ACK, and RST/ACK builders now encode IPv6 headers and checksums as well as IPv4. `TcpFrameParams<A>` defaults to `Ipv4Addr`, which preserves existing callers. The shared encoder rejects mixed address families and packets that exceed the relevant 16-bit IP length field.

All 58 packet tests passed on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 for `aarch64-apple-darwin`. Three new regressions verify independent IPv6 checksum sums and wire fields, the first oversized IPv6 payload, and mixed-family rejection. The maximum supported IPv6 TCP payload is 65515 bytes. Existing IPv4 full-byte fixtures and consumer checks passed, as did formatting and strict Clippy.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

The [dated benchmark](../benchmarks/network.md#2026-10-22--ipv6-capable-tcp-frame-encoder) compares the existing IPv4 path before and after the generic encoder change. Native binding checks passed. IPv6 SYN encoding and native TUN/application acceptance remain separate batches.
