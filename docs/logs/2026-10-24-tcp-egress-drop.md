# 2026-10-24 — TCP egress resolution supports silent rejection

- Type: change
- Area: splicetcp handshake resolution

## Problem

The egress resolver returned `Option<EgressConn>`. Both `None` and a closed result channel caused a TCP reset. Keeping the result pending also caused a reset after the seven-second handshake deadline. The contract could not express silent rejection.

## Change

The resolver returns `EgressOutcome` with `Connect`, `Reject`, and `Drop` variants. `Drop` removes the handshake without a response, socket, or pending receiver. The bridge consumes an outcome already delivered to its receiver before checking the handshake deadline. Pending results retain the existing timeout behavior. Repeated SYNs start resolution again. The handshake waker stops forwarding when the handshake removes its receiver.

This contract applies before the guest handshake completes. Consumers that accept the handshake before metadata inspection must handle established-flow rejection separately.

## Validation

The `tcp_bridge::tests::egress` tests cover repeated silent drops beyond the handshake capacity, a ready drop after the deadline, explicit rejection, a closed result channel, drop wakeup, and receiver cancellation on expiry. Existing handshake tests cover successful connections and pending expiry.

Validation ran from the ArcTap consumer workspace through its Rust 1.96 `devenv` environment:

- `devenv shell -- cargo test --manifest-path .source/arcbox/Cargo.toml -p splicetcp --all-features` — 128 tests passed.
- `devenv shell -- cargo fmt --manifest-path .source/arcbox/Cargo.toml -p splicetcp --check` — passed.
- `devenv shell -- cargo-clippy clippy --manifest-path .source/arcbox/Cargo.toml -p splicetcp --all-targets --all-features -- -D warnings` — passed.

The direct `cargo-clippy` command selects the configured Rust 1.96 tool. The host `cargo clippy` launcher selected Rust 1.99 and reported newer lints in existing tests.
