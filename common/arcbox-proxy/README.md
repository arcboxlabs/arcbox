# arcbox-proxy

Host socket egress, SOCKS5 and HTTP CONNECT clients, and inbound relay for ArcBox and host proxy consumers. This crate has no VM or device dependency.

Pass a hostname or an unbracketed IP address to `http_connect::connect_via_http_proxy`. The client brackets IPv6 addresses in both the CONNECT authority and the `Host` header. The returned stream preserves tunnel data that follows the response headers.

```rust
let tunnel = arcbox_proxy::http_connect::connect_via_http_proxy(
    "127.0.0.1:3128", "2001:db8::1", 443,
).await?;
```

Run the CONNECT regressions from the repository root with `devenv shell -- cargo test --locked -p arcbox-proxy --lib http_connect::tests`.
