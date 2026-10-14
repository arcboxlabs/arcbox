# 2026-10-14 — TCP promotion and receive batches preserve protocol state

- Type: change
- Area: `arcbox-packet`, `splicetcp`, `arcbox-net`, `arcbox-xnu-net`
- Base commit: `787a4d79b91fe7fe76c97d258d00942417f70da4`
- Related: [passive-open payload handling](2026-10-12-passive-open-payload.md), [splicetcp](../../common/splicetcp/README.md)

## Problems

TCP promotion published an initial receive window of 65535 to the connection sink before applying the passive-open completion ACK. An inline reader could observe that value even when the peer advertised zero. The active-open path did not apply the SYN-ACK window and retained the local Window Scale offer when the peer omitted that option.

The VM datapath and `tun_proxy` processed established TCP frames before handshake completion. Payload following the completion ACK in the same input batch could miss the established connection and then be discarded.

The source-aware IPv4 UDP batch syscall interface already existed, but `AsyncBatchDgram` did not expose that interface through Tokio readiness.

## Changes

`FastPathInit` makes the initial window explicit. Promotion initializes shared window state before publishing the stream. Passive-open completion windows use the negotiated scale; SYN-ACK windows remain unscaled. Window Scale parsing clamps peer values to the RFC 7323 maximum of 14 before any shift.

Both macOS consumers call `FrameClassifier::drain_tcp` to preserve arrival order across promotion and later payload. VM replies keep reliable delivery and the existing flush points. The host tunnel harness keeps its existing write policy.

`AsyncBatchDgram::recv_batch_from_v4` accepts caller-owned slots and returns source metadata through the existing batch receive implementation. Only `WouldBlock` clears readiness. A successful receive returns without another await; cancelling a pending receive does not consume a datagram. Empty slots return immediately. Darwin's silent truncation contract remains explicit in the API documentation.

## Regression evidence

Before the window fix, both new publication tests observed 65535 instead of zero from another thread inside `ConnSink::send_conn`. The tests pass after the fix and also check small windows, scaling negotiation, and later window updates.

Before the parser fix, an offered scale of 15 remained 15 instead of 14. Handshake regressions with a scale of 255 also triggered shift overflow during passive promotion and a later active-open window update. The parser regression and both handshake regressions pass after clamping.

The datapath regression runs `NetworkDatapath::run` with a guest datagram socketpair and a real loopback TCP upstream. The test queues the completion ACK, two payload segments, and FIN without yielding. The old two-pass drain timed out after two seconds. The ordered drain delivers the exact payload once, closes the upstream write direction, and acknowledges the payload and FIN without a guest retransmission.

UDP regressions check multiple sources, empty datagrams, slot boundaries, oversized datagrams, cancellation while idle, readiness after draining, and non-IPv4 error propagation.

## Validation

The targeted commands run on macOS through the repository's devenv environment:

```sh
devenv shell -- cargo test -p arcbox-packet -p splicetcp --all-features
devenv shell -- cargo test -p arcbox-net --all-features
devenv shell -- cargo test -p arcbox-xnu-net --all-features
```

On macOS with Rust 1.96.0, `arcbox-packet` passed 45 tests, `splicetcp` passed 129 tests, `arcbox-net` passed 104 tests, and `arcbox-xnu-net` passed 29 tests plus one doctest. Existing ignored tests retained their original markers. Each affected crate passed all-targets, all-features Clippy with `-D warnings`.

The final tree also passed the repository gates: `cargo fmt --check`, `cargo clippy --workspace --exclude arcbox-agent -- -D warnings`, `cargo xtask check-layers`, and `cargo test --workspace --exclude arcbox-agent`, all through `devenv shell`. The dependency `proc-macro-error2` emitted a future-incompatibility notice; project Clippy checks passed.

An isolated Linux arm64 container with Rust 1.96.1 passed 45 packet tests and 120 splicetcp tests using `cargo test --locked -p arcbox-packet -p splicetcp --all-features`. Source and build-file hashes matched the tested snapshot after the run. The existing container image did not include Clippy, so Linux Clippy was not run. The temporary container was removed after validation.

These regressions exercise protocol correctness and the real macOS datapath loop. They do not boot a VM or establish new product throughput, CPU, or memory results.
