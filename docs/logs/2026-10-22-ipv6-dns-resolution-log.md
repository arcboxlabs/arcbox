# IPv6 DNS resolution records

`DnsResolutionLog` stores `IpAddr` keys and accepts IPv4 or IPv6 addresses. IPv4, IPv4-mapped IPv6, and native IPv6 remain distinct entries. Both families use the same expiration and removal behavior.

`parse_dns_response_addresses` extracts A and AAAA answer addresses. The existing `parse_dns_response_a_records` entry point filters the result to IPv4 addresses, preserving its return type and behavior. See the [crate guide](../../common/arcbox-fakeip/README.md) for a minimal example.

Validation ran on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 on `aarch64-apple-darwin`. All 19 crate tests passed, with no failures or ignored tests. The regressions cover address-family identity, expiration, removal, mixed A/AAAA answers, truncated AAAA data, and compatibility with the IPv4-only parser.

Run the checks from the repository root:

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-fakeip --all-targets -- -D warnings
devenv shell -- cargo test --locked -p arcbox-fakeip
devenv shell -- cargo check --locked -p splicetcp -p arcbox-proxy --all-targets
```

All four checks passed. The checks use in-memory DNS records and packets; native VPN lifecycle and physical IPv6 connectivity require separate acceptance.
