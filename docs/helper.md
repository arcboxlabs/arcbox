# arcbox-helper

Privileged root helper for host mutations that the user-space daemon cannot
perform: `/usr/local/bin` CLI symlinks, `/var/run/docker.sock`, `/etc/resolver`,
`/etc/hosts` alias, local HTTPS CA trust, and utun/route setup.

## Independent version

`arcbox-helper` owns its **own** Cargo package version
(`app/arcbox-helper/Cargo.toml`), currently `1.2.0`. It is **not** tied to
`workspace.package.version`.

| Pin | Location |
|-----|----------|
| Helper package version | `app/arcbox-helper/Cargo.toml` → `version = "1.2.0"` |
| Workspace path-dep | root `Cargo.toml` → `arcbox-helper = { version = "1.2.0", path = ... }` (**no** `x-release-please-version`) |
| Daemon/CLI floor | `arcbox_constants::helper::MIN_HELPER_VERSION` |

`arcbox-helper --version` and the tarpc `version` RPC both print:

```text
arcbox-helper 1.2.0
```

Desktop and daemon parse that line with
`arcbox_constants::helper::parse_helper_version` and compare by semver triple
**within the same major** (a higher major is a wire break, not “newer is fine”).

### When to bump

Bump the helper version (and the three pins above **together**) only when the
**on-disk root binary** must be replaced:

- tarpc wire break / required new RPC method
- security fix in the privileged helper
- behavior the current daemon hard-requires

Ordinary daemon / CLI / guest / networking work must **not** bump the helper.
Otherwise every Desktop user gets an admin-password reinstall prompt.

release-please does **not** bump `app/arcbox-helper/Cargo.toml` (no
`x-release-please-version` on that pin). Helper bumps are manual.

A regression test (`helper_version_pins_are_aligned` in
`app/arcbox-helper/tests/connection_test.rs`) fails if the three pins drift.

### Admin password impact

Installing or upgrading the helper requires admin authorization (AppleScript /
`osascript` admin prompt, or `sudo abctl _install`). Once the on-disk helper
semver is **≥** the bundled helper, subsequent ArcBox app updates that only
bump the runtime (`0.4.x`) must **not** re-prompt.

## Threat model

### Socket mode `0666` / `SockPathMode` 438

`/var/run/arcbox-helper.sock` is world-connectable so the non-root daemon and
CLI can reach a root launchd service without setuid tricks.

**Authorization is not the filesystem mode.** Release builds run peer code-
signature verification on every accept (`peer_auth::verify`):

1. Prefer `LOCAL_PEERTOKEN` (audit token) — binds to the live connecting process.
2. Fall back to `LOCAL_PEERPID`.
3. `SecCodeCopyGuestWithAttributes` → `SecCodeCheckValidity` with
   strict dynamic-validation (without the static-only nested-code flag) and
   `identifier "…" and anchor apple generic and certificate leaf[subject.OU] = "TEAM"`.
4. Allow-list: `com.arcboxlabs.desktop{,.dev}{,.daemon,.cli}`.

Debug builds skip signature checks so ad-hoc local binaries can talk to a
manually started helper. **Never** ship a release helper with auth disabled.

Rejected peers are dropped before any tarpc dispatch; logs include
`source` (`audit_token` / `pid` / `none`) and `stage`.

Each accepted connection retains its kernel UID, matched signing identifier, and audit-token evidence in the handler. `VerifiedPeer::resolver_profile` returns a profile only for an audit-token-verified daemon signature. PID verification, CLI and Desktop signatures, and the debug bypass do not establish a resolver identity. Existing RPCs use connection-wide authorization.

### Mutation ownership rules

| Mutation | Create / replace | Delete |
|----------|------------------|--------|
| `cli_link` / `cli_unlink` | Target must be a **regular file** under `<ArcBox bundle>.app/Contents/MacOS/xbin/` (`/Applications` or `/Users`); existing `/usr/local/bin` entry replaced only if ArcBox-owned symlink | Only ArcBox-owned symlink |
| `socket_link` / `socket_unlink` | Target must parse as `SocketTarget` (`~/.arcbox` / `~/.arcbox-dev`); replace only ArcBox-owned symlink | Only ArcBox-owned symlink; never real sockets |
| `dns_install` / `dns_uninstall` | Requires the complete generated resolver contents, a regular file with one link, matching owner, no group/other write bits, and no ACL entries; atomically publishes mode 0644 | Removes only files that pass the same ownership checks; preserves foreign files and links |
| `hosts_alias_*` | Fixed `127.0.0.1 ArcBox # managed by arcbox-helper` line only | Lines carrying the marker only |
| `ca_install` / `ca_status` / `ca_remove` | Public DER certificate must pass the ArcBox root policy, including verified self-signature, critical DNS/IP constraints, and CA path length zero; stores the System Keychain copy and inspects TLS-server-only Admin trust | Exact certificate DER only; removal requires prior Admin trust revocation and preserves other roots |

The helper never receives a CA private key or a certificate file path. `ca_install` stores the public certificate without changing trust. `ca_status` checks both the System Keychain certificate and the precise Admin trust policy; existing User trust cannot satisfy that check. `ca_remove` refuses deletion until Admin trust is revoked. Trust mutations run in the login user's GUI session through native Security.framework APIs. Desktop invokes the bundled `abctl tls trust` for an explicit HTTPS action or a browser request to an observed managed service. Cancellation returns CLI status 130 and leaves containers running. Certificate renewal invokes `abctl tls rotate`; the daemon publishes the replacement after the CLI completes native trust migration. Correct existing trust skips native mutation. Root helper installation authorization cannot replace CA trust authorization. See [ADR 0006](adr/0006-authorize-local-ca-in-user-session.md) and [ADR 0007](adr/0007-local-https-independent-of-startup.md).

The resolver storage module (`server/mutations/dns/storage.rs`) validates directory access and file metadata before applying the caller's complete-content ownership predicate. Resolver mutation requires a directory owned by the helper user, without group/other write bits or ACL entries. The helper rejects a symlink at that directory and opens resolver files without following links. Every connection shares one `ResolverStore`, which holds the resolver directory and serializes ownership checks with install and uninstall. Tests inject a temporary directory into the same store implementation. Publication uses `arcbox-atomic-file::write_public` to set mode 0644 before the atomic rename. These checks assume trusted `/private/etc` ancestry; they do not arbitrate changes by another root process. Per-instance leases and revision-safe cleanup remain separate unfinished work.

`is_arcbox_owned` (shared with `CliTarget`) rejects relative paths, `..`, and
anything outside `/Applications/` or `/Users/` without an
`<ArcBox bundle>.app/Contents/MacOS/xbin/` segment. The bundle name is part of
the rule (`ArcBox.app`, `ArcBox Dev.app`, a Finder-renamed `ArcBox 2.app`):
OrbStack links its CLI tools from the same `xbin` layout, and an older
layout-only check replaced and deleted its links (#715).

### Client-wide compatibility gate

Every mutation-capable `Client::connect` / `Client::connect_to` call checks the
helper's `version` RPC before returning a client. If the helper is unreachable,
unparseable, outside the supported wire major, or older than
`MIN_HELPER_VERSION`, no mutation client is returned. This protects every
consumer—including daemon `self_setup`, route reconciliation, and CLI helper
operations—from driving new tarpc ordinals into a legacy binary.

Diagnostics use the non-mutating `Client::probe_version` path so `abctl doctor`
can still report the version of an incompatible helper without exposing a
client capable of sending privileged mutations.

## Local E2E (no root)

Debug builds honour `ARCBOX_HELPER_TEST_ROOT`. When set, privileged paths are
rewritten under that directory so the real helper binary can be exercised
without touching the live system:

```bash
cargo test -p arcbox-helper --test e2e_fs_test
```

Release builds **ignore** the env var (production must never be redirected).
CA RPCs reject a filesystem test root because Keychain mutations cannot be redirected into that directory. The helper's ignored native storage test requires root and checks exact-certificate storage, idempotency, and sibling isolation. Native trust lifecycle checks run separately in the login user's GUI session and require macOS authorization.

Also covered by unit tests in the helper binary (`--bins`) and mock tarpc
integration tests (`--tests` excluding `e2e_fs_test`).

## Wire errors

RPC failures use structured [`HelperError`](../app/arcbox-helper/src/error.rs)
(`Result<(), HelperError>` over bincode). Variants are **append-only** (index
encoded). Clients match on `HelperError::code()` / enum arms; `Display` remains
human-readable for logs.

## Peer auth implementation

Release builds use the `security-framework` crate
(`GuestAttributes` + `SecCode` + `SecRequirement`) rather than hand-rolled
Security.framework FFI. Debug builds skip signature checks.

The peer-authentication tests verify daemon profile selection and reject PID-only, CLI, Desktop, and unsigned resolver identities. Release Clippy must also pass because debug builds do not compile the release signature-verification branch: `cargo clippy --release -p arcbox-helper --bin arcbox-helper -- -D warnings`.

## Non-goals (not 1.0.0 blockers)

- Batch `cli_link` RPC — call sites link a fixed small set of tools.
- Helper `health` RPC — `version` + connect success is enough for doctor.
- Doctor codesign detail dump — useful later; not required for correctness.

## Related paths

See also [data-directories.md](data-directories.md) and
[code-signing-troubleshooting.md](code-signing-troubleshooting.md).

| Path | Role |
|------|------|
| `/usr/local/libexec/arcbox-helper` | Installed root binary |
| `/Library/LaunchDaemons/com.arcboxlabs.desktop.helper.plist` | launchd job |
| `/var/run/arcbox-helper.sock` | RPC socket |
| `/var/log/arcbox/helper.log` | Helper log |
| `/usr/local/bin/docker*` | CLI convenience symlinks into app xbin |
| `/var/run/docker.sock` | Optional symlink to `~/.arcbox/run/docker.sock` |
