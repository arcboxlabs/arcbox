# 2026-10-08 — The helper manages local HTTPS trust

- Type: change
- Area: local CA, privileged helper, daemon startup, CLI uninstall
- Related: [ADR 0005](../adr/0005-helper-manages-local-ca-trust.md)

## Behavior

Helper 1.2.0 adds public-certificate install, status, and removal RPCs. The helper validates the ArcBox root policy and self-signature before modifying TLS-server-only Admin trust or the System Keychain. The helper receives no private key or caller-selected path. Certificate removal compares exact DER and preserves other roots.

Canonical production startup verifies trust before VM boot. A certificate replacement revokes old Admin trust before publishing the new root. Existing User trust is removed by explicit untrust or uninstall; macOS can require authentication for that legacy operation. Development and isolated instances do not install system trust automatically.

Uninstall stops the application and daemon, revokes trust, deletes the profile identity, and then removes the helper. A failed prerequisite preserves the helper and data for retry. The Homebrew hook also revokes trust before application removal. User Keychain identity lookup and deletion explicitly exclude the helper's System Keychain copy.

## Verified checks

- `cargo test -p arcbox-local-ca --lib -- --include-ignored`: 6 tests pass, including actual Keychain lifecycle and strict public-root validation.
- `cargo test -p arcbox-helper`: 80 tests pass; the root-only native trust lifecycle remains ignored in this command.
- `cargo test -p arcbox-cli --bin abctl commands::uninstall`: 8 tests pass, including cleanup ordering and preservation after shutdown, revocation, or identity-deletion failure.
- `cargo test -p arcbox-daemon --bin arcbox-daemon recovery::tests`: 4 tests pass, including production and isolated-profile ownership.
- Workspace formatting and focused host Clippy pass. Guest Clippy passes for `aarch64-unknown-linux-musl`.
- `cargo xtask check-layers`: 67 members, 187 dependency edges, and 2 existing grandfathered edges.
- Native macOS release binaries and Linux guest binaries build. The staged application, including helper 1.2.0, passes deep strict code-signature verification.

## Installation checkpoint

Before installation, the production CA has User trust and no Admin trust. The signed package is ready. Administrator authentication is required before the root-only native lifecycle check and installed-helper upgrade can run. Production trust and running containers remain unchanged at this checkpoint.
