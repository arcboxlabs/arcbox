# IPv6 system-proxy endpoints

System proxy parsing previously truncated host values at the second colon, and TCP/UDP egress formed ambiguous IPv6 host:port strings. The parser now splits each scutil field at its first colon and matches the exact key. `ProxyConfig` implements the shared authority formatter used by proxy URLs, detection logs, TCP selection, and SOCKS5 UDP control connections.

The dedicated `xuan-work-mbp` test Mac passed 21 arcbox-fakeip tests, 33 arcbox-proxy tests, and 136 splicetcp tests with all features and Rust 1.96.0. New coverage preserves IPv6 scutil values, rejects key-prefix collisions, checks every TCP system-proxy selection branch, and establishes a real IPv6 loopback SOCKS5 control connection with UDP echo through its separately negotiated IPv4 relay. Existing IPv4 and hostname behavior also passed.

Run each crate with `devenv shell -- cargo test --locked -p <crate> --lib --all-features`. Strict Clippy passed for all three crates with `--all-targets --all-features -- -D warnings`; workspace formatting and consumer compilation also passed. Validation used a fresh build directory. System proxy settings were not modified; original App, NE, VPN, DNS, and interface state remained unchanged. Native application acceptance remains pending.
