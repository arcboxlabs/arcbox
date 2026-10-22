# Tokio descriptor registration

The workspace now requires Tokio 1.53.3 or later. Descriptor adapters use `AsyncFd::register` and `register_with_interest`, which replace the deprecated constructors and require explicit descriptor lifetime guarantees. Each call documents the descriptor owner and the deregistration order. The adapters preserve their existing I/O and shutdown behavior.

Cargo resolved the lockfile with `cargo update -p tokio --precise 1.53.3`. Tokio and `tokio-macros` are the only packages with new versions. Cargo also normalized existing dependency references and removed four unreferenced Windows packages. The workspace dependency version was edited directly because Cargo 1.96 has no option for editing that workspace field.

Validation ran on the dedicated `xuan-work-mbp` test Mac with Rust 1.96.0 on `aarch64-apple-darwin`. Formatting and strict Clippy passed for all eight affected crates. All 29 selected tests passed: batch datagrams (2), raw and Unix stream shutdown (3), ICMP echo (2), serial pipes (7), socketpair datapath (9), and mDNS logic (6). The new raw descriptor test covers bidirectional bytes, half-close, close-on-drop, and writes after a no-op shutdown.

Run the checks from the repository root:

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --locked -p arcbox-proxy -p arcbox-xnu-net -p arcbox-transport -p arcbox-engine -p arcbox-daemon -p arcbox-net -p arcbox-vmnet -p arcbox-vz --all-targets --features arcbox-xnu-net/tokio,arcbox-vmnet/vmnet -- -D warnings
devenv shell -- cargo test --locked -p arcbox-xnu-net --features tokio --lib async_batch::tests
devenv shell -- cargo test --locked -p arcbox-transport --lib vsock::stream::tests
devenv shell -- cargo test --locked -p arcbox-proxy --lib egress::icmp::tests
devenv shell -- cargo test --locked -p arcbox-engine --lib machine::serial::tests
devenv shell -- cargo test --locked -p arcbox-net --lib darwin::datapath_loop::tests
devenv shell -- cargo test --locked -p arcbox-daemon --bin arcbox-daemon mdns::mirror::tests::
devenv shell -- cargo test --locked -p arcbox-daemon --bin arcbox-daemon mdns::tests::only_local_domains_are_announced -- --exact
```

The checks use Unix socketpairs, pipes, loopback ICMP, and in-memory mDNS records. The raw stream test does not exercise a guest vsock transport. VM, TUN, real Bonjour registration, and native VPN lifecycle require separate acceptance.
