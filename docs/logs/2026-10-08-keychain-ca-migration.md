# 2026-10-08 — The host signs local HTTPS certificates with a Keychain CA

- Type: change
- Area: `arcbox-local-ca`, daemon, guest HTTPS, CLI uninstall
- Related: [ADR 0004](../adr/0004-local-ca-signs-in-macos-keychain.md)

## Behavior

The macOS daemon creates a new per-profile EC P-256 CA in the file Keychain. Before boot, the daemon removes the exposed legacy key and revokes the old root's user trust. The daemon publishes only the new public certificate. Users must run `abctl tls trust` after migration.

The guest generates leaf keys in memory. A parked host connection on vsock port 2202 carries one length-prefixed CSR and certificate, each bounded to 8192 bytes. The host verifies the CSR signature and accepts exactly one DNS name below `arcbox.local`. The host fixes certificate usage and lifetime. The guest retains TLS termination and container port 443 routing. Agent protocol version 7 requires this signing contract.

The Keychain certificate lookup uses the key's public-key hash because macOS does not retain the supplied label on certificate insertion. Deletion uses a persistent certificate reference because a certificate parsed from DER is not a deletable Keychain item. Key deletion selects all matching items. Path labels remain stable when the TLS directory is absent, so uninstall can remove the profile's key after local files have been deleted.

The legacy file-backed CA generator and guest CA signer are removed. Automatic signing requires a macOS host; this change does not provide a Linux host key store.

## Verified checks

- `cargo test -p arcbox-local-ca --lib -- --include-ignored`: 4 tests pass, including real Keychain migration, stable reopen, profile isolation, deletion after directory removal, CSR policy, certificate privileges, and root DNS/IP constraints.
- `cargo test -p arcbox-core --lib`: 125 tests pass.
- `cargo test -p arcbox-cli --bin abctl commands::uninstall`: 6 tests pass.
- `cargo test -p arcbox-cli --bin abctl commands::tls`: 2 tests pass.
- Host Clippy passes for the CLI, daemon, core, local CA, and e2e targets. Guest Clippy passes for `aarch64-unknown-linux-musl`.
- `cargo xtask check-layers`: 67 members and 186 edges checked; 2 existing grandfathered edges.
- Host and guest release binaries build. Boot bundle 0.8.8 passes the pinned manifest and artifact checks.

## Live verification

The production daemon was stopped before each isolated VM run. These commands passed with the rebuilt release daemon and guest agent:

```sh
SKIP_BUILD=1 ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test virtio_debug -- --ignored --nocapture
SKIP_BUILD=1 ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test boot_assets -- --ignored --nocapture
SKIP_BUILD=1 cargo test -p arcbox-e2e --test local_https -- --ignored --nocapture
```

The HTTPS test sends verified requests across the guest bridge to container port 443, with an HTTP upstream on port 8080. Two SNI names exercise new signing connections; repeating the first name exercises the guest cache. The test asserts that no shared root key exists. The test deletes its isolated CA without adding user trust.

Two test setup errors were corrected before the successful run: current Docker exposes the bridge address at `NetworkSettings.Networks.bridge.IPAddress`, and current Alpine requires the existing smoke-test `nc` server pattern because its base BusyBox no longer contains `httpd`.

## Installed profile migration

The installed macOS application was updated with a matched daemon, CLI, and guest agent. The host binaries were built with the desktop repository's native macOS build target, which rejects Nix dynamic-library dependencies. The nested daemon and outer application passed `codesign --verify --deep --strict` with production entitlements. The original application was retained outside the installation directory.

The production VZ daemon reached `READY` through `WatchSetupStatus`. Migration removed `tls/ca-key.pem`. Security.framework confirmed that the old public certificate has no user trust. `abctl tls trust` installed TLS trust for the new certificate.

A temporary HTTP container on port 8080 returned the expected body through its real `https://<name>.arcbox.local` URL on port 443. The check used macOS `/usr/bin/curl` with system trust, without `--cacert` or `--insecure`. The application was then quit normally and restarted. The public CA certificate remained byte-identical, and the same HTTPS check passed with a fresh guest and leaf key.

All 8 containers that were running before the migration were restored. Containers that Docker restart policies unexpectedly started were stopped to match the saved running-container set. Temporary HTTPS containers and the isolated tests' Keychain identities were removed.
