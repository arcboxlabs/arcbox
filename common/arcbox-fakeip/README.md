# arcbox-fakeip

Portable DNS resolution records, system proxy detection, and proxy policy selection for ArcBox and host proxy consumers. This crate has no VM or device dependency.

Use `dns_log::parse_dns_response_addresses` to extract A and AAAA answer addresses. The existing `parse_dns_response_a_records` function returns only IPv4 addresses. `DnsResolutionLog` accepts either IP family and preserves IPv4, IPv4-mapped IPv6, and native IPv6 as distinct keys.

```rust
use arcbox_fakeip::dns_log::DnsResolutionLog;
use std::net::Ipv6Addr;

let log = DnsResolutionLog::new();
let address = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
log.record("service.example", &[address]);
assert_eq!(log.lookup(address).as_deref(), Some("service.example"));
```

Run the crate tests from the repository root with `devenv shell -- cargo test --locked -p arcbox-fakeip`.

`proxy_detect::ProxyConfig` formats proxy authorities through `Display`, including bracketed IPv6 addresses. System proxy parsing matches exact scutil keys and retains colons in host values.
