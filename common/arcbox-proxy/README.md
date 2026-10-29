# arcbox-proxy

Host socket egress, SOCKS5 and HTTP CONNECT clients, and inbound relay for ArcBox and host proxy consumers. This crate has no VM or device dependency.

The `egress` module and its `HostEgress` UDP/ICMP frame dispatcher require Unix. SOCKS5 and HTTP CONNECT clients, including SOCKS5 UDP associations, remain portable. The inbound listener manager and its command channel are also portable. Windows has no host ICMP backend in this crate.

Pass a hostname or an unbracketed IP address to `http_connect::connect_via_http_proxy`. The client brackets IPv6 addresses in both the CONNECT authority and the `Host` header. The returned stream preserves tunnel data that follows the response headers.

```rust
let tunnel = arcbox_proxy::http_connect::connect_via_http_proxy(
    "127.0.0.1:3128", "2001:db8::1", 443,
).await?;
```

Run the CONNECT regressions from the repository root with `devenv shell -- cargo test --locked -p arcbox-proxy --lib http_connect::tests`.

System SOCKS5 UDP proxy endpoints use the same `ProxyConfig` authority formatting as TCP egress, including IPv6 control endpoints. The UDP relay address is negotiated independently through SOCKS5.

Inbound UDP associations use the host listener address and client address as their identity. Datagrams from one live association reuse a guest-visible source port. Replies carry the client address through one listener-owned channel. Listener removal closes that channel and releases the socket before returning. Idle associations expire after 60 seconds; listener replacement creates a new association. Source-port allocation skips live flows and rejects exhaustion without replacing another client.

`HostEgress` accepts IPv4 frames only. Both the Ethernet type and IP version must identify IPv4 before inbound association matching or host socket dispatch.
