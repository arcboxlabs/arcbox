# HTTP CONNECT IPv6 authority

The HTTP CONNECT client now uses `SocketAddr` formatting for IP targets. An unbracketed input such as `2001:db8::1` produces `[2001:db8::1]:443` in both the request line and the `Host` header. Hostname targets retain their existing format. See the [crate guide](../../common/arcbox-proxy/README.md) for usage.

Validation ran on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 on `aarch64-apple-darwin`. Both CONNECT tests passed. The IPv6 regression checks exact request bytes, reads tunnel data appended to the response headers, and sends data back through the tunnel. The hostname regression checks bidirectional tunnel traffic.

Run the checks from the repository root:

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-proxy --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-proxy --lib http_connect::tests
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

All four checks passed. The tests use an IPv4 loopback proxy to inspect the encoded IPv6 destination. Physical IPv6 connectivity and native VPN lifecycle require separate acceptance.
