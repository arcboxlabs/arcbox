# 2026-10-12 — Passive-open completion preserves payload and receive window

- Type: change
- Area: `common/splicetcp`
- Base commit: `4064dfda9a0f80ea62bc61b0e4fe6db29810ebb4`
- Related: [splicetcp](../../common/splicetcp/README.md)

This entry preserves the original local regression evidence. Upstream integration checks are reported separately; these results do not measure VM throughput.

## Problem

`try_complete_handshake` consumed the passive-open final ACK after promotion without processing that frame. An ACK carrying data therefore lost its payload until the peer retransmitted. The bridge also ignored the receive window in that ACK.

The regression uses a real loopback upstream and the classifier boundary. It submits the completion packet once, without a preceding pure ACK or an injected retransmission. Before the fix, the handshake completed but the upstream received zero of the 44 payload bytes. A separate test showed that a zero window remained at the promotion default of 65535.

## Change

After passive-open promotion, the bridge passes the completion frame to the existing fast path. Both `drain_tcp` and `drain_handshake` receive any resulting ACK. Empty replies remain absent from the reply list. The active-open SYN-ACK path is unchanged because its SYN sequence and window semantics differ.

The fast path controls sequence overlap, missing prefixes, retained upload bytes and socket writes. Only bytes accepted by the host socket advance the cumulative ACK. The completion frame also updates the scaled receive window, including zero windows.

## Evidence

The four new classifier regressions failed on the base implementation and passed after the fix. They cover single-delivery payload through polled and inline promotion, shared ACK state, zero and scaled windows, the handshake-only drain contract, and a missing prefix that must arrive before the queued suffix can be forwarded.

The original 2026-10-12 validation ran through the ArcTap devenv environment with Rust 1.96.0 on macOS. Run these commands with the ArcBox manifest path when the shell starts outside ArcBox:

```sh
cargo fmt -p splicetcp --check
cargo clippy -p splicetcp --all-targets --all-features -- -D warnings
cargo test -p splicetcp --all-features
```

That validation passed all 127 splicetcp tests, including existing short-write, WouldBlock, write-error, active-open and loss-recovery tests. ArcTap's related library suites also passed: 33 FFI tests and 46 inbound tests, with one pre-existing ignored inbound test. The new handshake tests use valid IPv4/TCP checksums and packets no larger than 1500 bytes.

## Limits

This local regression proves a conditional data-handoff defect and its correction. The tests do not establish throughput gains or explain the previously observed approximately 237 ms mixed-load latency tails. At the time of the original regression, product acceptance still needed to measure the separately built candidate against retained build 10039 before selecting a deployed version.
