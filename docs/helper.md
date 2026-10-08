# arcbox-helper

Privileged root helper for host mutations that the user-space daemon cannot
perform: `/usr/local/bin` CLI symlinks, `/var/run/docker.sock`, `/etc/resolver`,
`/etc/hosts` alias, local HTTPS CA storage, and utun/route setup.

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
**within the same major**. A different major may change required helper behavior.

### Coordinated helper updates

Keep the helper package and minimum version at `1.2.0` for this paired deployment. Ship the helper and its daemon/CLI consumers together when RPC or required behavior changes. Do not add compatibility decoders or automatically bump versions for protocol changes.

A matching semver alone does not establish readiness. The installer requires the current JSON RPC transport, the same-major version floor and bundled-version policy, and an explicit selected-resolver check. release-please does not manage the helper version pins.

A regression test (`helper_version_pins_are_aligned` in
`app/arcbox-helper/tests/connection_test.rs`) fails if the three pins drift.

### Admin password impact

Installing or upgrading the helper requires admin authorization (the Desktop system prompt or `sudo abctl _install`). Desktop skips authorization only when the installed helper answers compatible JSON RPCs, meets the bundled same-major version requirement, and has no selected legacy resolver files to migrate. Runtime-only updates that satisfy these checks do not re-prompt.

`abctl _install --check --no-daemon --no-shell --helper-path PATH --user-uid UID --data-dir ABSOLUTE_PATH` prints exactly one JSON result, such as `{"helper_ready":true,"legacy_roots":[]}`. Missing or incompatible helper services report `helper_ready: false`; inspection errors return a nonzero exit status. Logs use stderr. The installer repeats the check after bootstrap and keeps the selected daemon stopped when the result is not ready.

`arcbox-helper --check-resolvers [ROOT...]` reads only the selected resolver roots and prints each exact legacy root on its own line. The check requires root-owned files and directory metadata even when the caller is a login user. It does not start the service or change files. An empty root set does not inspect `/etc/resolver`.

Before replacing the executable, the privileged installer runs the bundled helper's `--prepare-install [ROOT...]` command. The command checks `launchctl bootout`, confirms the exact helper label is absent from `launchctl list`, and waits up to 50 seconds for root helper processes at the installed executable path to exit. The command excludes its own maintenance process and propagates inspection errors. Replacement never copies the installed executable onto itself.

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

Each accepted connection retains its kernel UID, matched signing identifier, and audit-token evidence in the handler. `VerifiedPeer::resolver_profile` returns a profile only for an audit-token-verified daemon signature. PID verification, CLI and Desktop signatures, and the debug bypass do not establish a resolver identity. Other RPCs use connection-wide authorization.

### Mutation ownership rules

| Mutation | Create / replace | Delete |
|----------|------------------|--------|
| `cli_link` / `cli_unlink` | Target must be a **regular file** under `<ArcBox bundle>.app/Contents/MacOS/xbin/` (`/Applications` or `/Users`); existing `/usr/local/bin` entry replaced only if ArcBox-owned symlink | Only ArcBox-owned symlink |
| `socket_link` / `socket_unlink` | Target must parse as `SocketTarget` (`~/.arcbox` / `~/.arcbox-dev`); replace only ArcBox-owned symlink | Only ArcBox-owned symlink; never real sockets |
| `resolver_lease` | Requires an audit-token-verified daemon profile, immutable startup nonce, increasing revision, and exclusive exact roots; publishes the complete requested set | On partial-publication failure or EOF, removes only files matching this connection's exact instance and revision stamp |
| `hosts_alias_*` | Fixed `127.0.0.1 ArcBox # managed by arcbox-helper` line only | Lines carrying the marker only |
| `ca_install` / `ca_status` / `ca_remove` | Public DER certificate must pass the ArcBox root policy, including verified self-signature, critical DNS/IP constraints, and CA path length zero; stores the System Keychain copy and inspects TLS-server-only Admin trust | Exact certificate DER only; removal requires prior Admin trust revocation and preserves other roots |

The helper never receives a CA private key or a certificate file path. `ca_install` stores the public certificate without changing trust. `ca_status` checks both the System Keychain certificate and the precise Admin trust policy; existing User trust cannot satisfy that check. `ca_remove` refuses deletion until Admin trust is revoked. Trust mutations run in the login user's GUI session through native Security.framework APIs. Desktop invokes the bundled `abctl tls trust` for an explicit HTTPS action or a browser request to an observed managed service. Cancellation returns CLI status 130 and leaves containers running. Certificate renewal invokes `abctl tls rotate`; the daemon publishes the replacement after the CLI completes native trust migration. Correct existing trust skips native mutation. Root helper installation authorization cannot replace CA trust authorization. See [ADR 0006](adr/0006-authorize-local-ca-in-user-session.md) and [ADR 0007](adr/0007-local-https-independent-of-startup.md).

The resolver storage module (`server/mutations/dns/storage.rs`) validates directory access and file metadata before applying the caller's complete-content ownership predicate. Files must be regular, have one link, match the helper user, and have no group/other write bits or ACL entries. Resolver mutation requires a directory owned by the helper user, without group/other write bits or ACL entries. The helper rejects a symlink at that directory and opens resolver files without following links. Every connection shares one `ResolverStore`, which holds the resolver directory and serializes ownership checks with lease publication and cleanup. Tests inject a temporary directory into the same store implementation. Publication uses `arcbox-atomic-file::write_public` to set mode 0644 before the atomic rename. These checks assume trusted `/private/etc` ancestry; they do not arbitrate changes by another root process.

`resolver_lease` binds the kernel UID and verified daemon profile to one helper connection, startup nonce, and increasing revision. Roots are validated, sorted, and deduplicated without widening to an ancestor. `.local` itself is forbidden. Exact roots remain exclusive until the previous connection completes cleanup; nested roots may belong to different connections.

A successful lease response confirms publication of every requested root and withdrawal of omitted roots. Every failed lease request permanently invalidates that connection. Validation and preflight failures preserve existing files until EOF. Partial publication failure immediately attempts cleanup of both the previous and attempted revision, including a write that renamed the file before returning `DurabilityUncertain`. Cleanup continues across paths and reports all errors. EOF drains in-flight requests before exact-stamp cleanup. Request task failures are reported after cleanup completes. Failed EOF cleanup retains ownership and blocks new claims until helper restart; there is no automatic cleanup retry.

At helper startup, recovery removes only resolver files with the complete canonical lease stamp and the existing private-file safety properties. Recovery preserves unleased resolver files, malformed stamps, writable files, and links. Live updates accept only absent files or the current connection's exact file contents; another connection's stamp is never an ownership grant.

`resolver_watch` requires an audit-token-verified daemon and returns the complete sorted root set with a helper-lifetime global epoch. Ownership is relative to the calling connection; private connection identifiers and nonces remain internal. Active roots carry the loopback DNS port and the last completely accepted lease revision. Failed or retiring connections publish their retained root union as unavailable, without a port or revision. Consumers must select the longest inclusive instance root before resource ownership lookup. An unavailable root forbids forwarding and broader ownership fallback.

The watch epoch includes accepted lease metadata. The separate `authority_revision` advances only when root ownership, availability, or a DNS endpoint changes. The daemon preserves that lifetime change even when a watch coalesces withdrawal and re-acquisition into an identical final snapshot. Repeated snapshots and metadata-only updates preserve existing DNS/TLS revisions.

Lease consumers use `Client::connect_owned_to` and retain its dispatcher handle through connection shutdown. The constructor retains abort ownership during version verification, so cancellation and incompatible versions close the transport. The handle reports idle EOF without requiring another RPC. Ordinary `connect` and `connect_to` keep their existing detached-dispatch API after successful verification.

The daemon's resolver service leases its configured suffix and every raw exact or wildcard `.local` base, including unavailable and conflicting resources. Explicit initial setup waits for a confirmed lease; canonical setup retries on a fresh connection after failure. The service marks known and attempted roots unavailable, withdraws mDNS, then closes and joins the helper connection. Startup cancellation and normal shutdown join the same retained task. Legacy unleased resolver files require the explicit installation cutover before this service can acquire them.

The initial watch uses `None`; a different epoch returns immediately. An unchanged watch waits at most 30 seconds, while the client allows 35 seconds. The helper releases the mutation lock before waiting. Transport EOF or error marks the connection as retiring and cancels pending watches before joining in-flight request tasks and releasing resolver files. This observation occurs while polling the underlying transport because tarpc can remain pending after EOF while a watch is in flight. Existing request cancellation remains active; synchronous resolver mutations complete once started. A reconnect must discard the previous snapshot and request a new complete snapshot.

`is_arcbox_owned` (shared with `CliTarget`) rejects relative paths, `..`, and
anything outside `/Applications/` or `/Users/` without an
`<ArcBox bundle>.app/Contents/MacOS/xbin/` segment. The bundle name is part of
the rule (`ArcBox.app`, `ArcBox Dev.app`, a Finder-renamed `ArcBox 2.app`):
OrbStack links its CLI tools from the same `xbin` layout, and an older
layout-only check replaced and deleted its links (#715).

### Client-wide compatibility gate

Every mutation-capable `Client::connect` / `Client::connect_to` call checks the
helper's `version` RPC before returning a client. If the helper is unreachable,
unparseable, outside the supported package major, or older than
`MIN_HELPER_VERSION`, no mutation client is returned. This protects every
consumer—including daemon `self_setup`, route reconciliation, and CLI helper
operations—from using unsupported helper behavior.

The Unix transport uses length-delimited JSON frames. RPC methods and error variants use names, so deleting a method cannot redirect another request by enum index. Invalid JSON, unknown methods, and old Bincode frames close the connection before dispatch. There is no alternate decoder. Deploy the helper, daemon, and CLI together; the helper package version remains independent of the guest protocol.

Diagnostics use the non-mutating `Client::probe_version` path so `abctl doctor` can report an incompatible JSON helper's version without exposing a mutation client. A Bincode helper reports a connection or decoding failure instead.

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
(`Result<(), HelperError>` over JSON). Variants use names on the wire.
Clients match on `HelperError::code()` / enum arms; `Display` remains
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

### Installer target

`abctl _install --user-uid UID --data-dir ABSOLUTE_PATH` selects the login user and daemon data directory explicitly. Without `--user-uid`, the installer uses `SUDO_UID` or the current UID. UID 0 is not a login-user target. The installer reads the user's home from the account database. Privileged installation skips shell integration; run `abctl setup install` as the login user.

Keep the selected daemon stopped until installation finishes. Desktop unregisters the selected daemon and checks its lock before requesting authorization. The privileged installer checks the same lock and retains an exclusive lock when the file exists. A missing lock only indicates current absence; the installer does not create the daemon's lock. The installer never reads or signals a PID from this user-owned file.

`--resolver-root ROOT` may be repeated to select exact legacy roots. Explicit roots replace the profile default: production selects only `arcbox.local`; development selects none. Maintenance reserves every distinct legacy UDP port before stopping the helper, reinspects the entire selected batch, and removes files only when the batch remains identical. A busy port, stop failure, or changed batch preserves every selected file. No namespace scan or ancestor inference occurs.
