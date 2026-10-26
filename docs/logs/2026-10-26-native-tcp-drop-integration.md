# 2026-10-26 — Native TCP ownership and silent rejection share one bridge

- Type: integration
- Area: packet parsing, TCP lifecycle, and egress resolution

## Problem

The ArcTap dependency history ended at `6958d54` with IPv6 packet views, managed native sockets, and explicit teardown. The ArcBox master history included receive-window initialization before inline promotion and silent rejection. Replacing either history would remove a required behavior from the shared bridge.

## Change

The merge retains both histories. `EgressOutcome` distinguishes connection, reset, and silent drop. Both connected variants retain `NativeTcp` ownership. Controlled connections remain on the polled path. IPv4 and IPv6 abandonment use the same validated TCP view as established input.

Explicit abandonment takes priority over native cancellation during handshake polling, final ACK handling, and established polling. The primary descriptor closes and releases its owner without emitting a FIN or RST. Established discard state still acknowledges contiguous payload. Ordinary cancellation and explicit abort retain their existing state-dependent teardown behavior.

`FastPathInit` publishes the initial receive window before a reader receives the promoted stream. The merge preserves the Window Scale limit of 14 and the IPv4 UDP receive implementation from master.

## Validation

The targeted `splicetcp` suite with `tokio-frame-sink` passed 191 tests, with one manual benchmark ignored. Coverage includes cancellation and abandonment at queued, SYN-ACK, and final ACK boundaries; managed IPv6 teardown; final ACK payload delivery; inline receive-window initialization; and exactly-once abort accounting.

The following merge gates passed:

- Workspace formatting and Clippy with `-D warnings`, excluding the guest-only `arcbox-agent` from Clippy.
- `check-layers`: 68 members, 190 edges, and 2 grandfathered edges.
- Workspace tests excluding `arcbox-agent`: 3290 passed, 0 failed, and 257 ignored.
- `splicetcp` Clippy with all targets, all features, and `-D warnings`.

Validation uses the repository's Rust 1.96 `devenv` toolchain. No daemon or test-machine runtime was started for this merge.
