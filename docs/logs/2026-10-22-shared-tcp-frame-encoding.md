# Shared TCP frame encoding

ACK, data, FIN/ACK, and RST/ACK frames now share one IPv4 encoder. The encoder writes each checksum once. The builders preserve reserved zero fields, flags, receive windows, sequence numbers, and the IPv4 payload limit. The partial-checksum GSO and SYN builders retain their existing implementations.

All 55 packet tests passed on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 for `aarch64-apple-darwin`. The full-byte fixture covers payload lengths 0 through 65, odd and even MTU sizes, 32 KiB, and the IPv4 maximum. Two new regressions cover control-frame windows and the first oversized payload. Formatting, strict Clippy, and all-target consumer checks passed.

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-packet --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-packet --lib
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

The before/after microbenchmark records mixed timing changes, including slower results at some payload sizes. See [the dated measurement](../benchmarks/network.md#2026-10-22--shared-tcp-frame-encoding). Native binding checks passed. TUN, VM, and native application acceptance remain separate.
