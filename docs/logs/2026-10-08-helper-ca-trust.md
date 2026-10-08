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

## Live validation: deployment blocked

The native lifecycle test failed on macOS 26.4 (25E246) with SIP enabled. The test ran through an administrator-authorized shell and passed its effective-UID-zero assertion. The first `install` returned `The authorization was denied since no user interaction was possible.` The test assertions remain unchanged.

The checked-in reproduction is `server::mutations::ca::tests::native_trust_lifecycle_preserves_other_roots`. Build the `arcbox-helper` binary test target, then run that exact ignored test as root. At 11:58:58, `authd` logged successful preauthorization followed by a rejection when `trustd` requested `com.apple.trust-settings.admin`: `session has no ui access`, error `-60007`.

The local authorization rule requires an entitlement or administrator authentication. [Apple's Big Sur 11.0.1 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-release-notes) explicitly state that root alone is insufficient to modify certificate trust. [Apple DTS confirms](https://developer.apple.com/forums/thread/671582) that a native call from an application's GUI context can present authorization UI. The older SDK header's root exemption is not a valid product guarantee.

This result invalidates ADR 0005's assumption that helper installation authorization removes the need for certificate-trust authorization. The current candidate always rewrites Admin trust before VM boot. Installing this candidate would therefore block production startup on this host. Do not deploy this candidate before the authorization flow is corrected and validated.

The installer stopped before upgrading the helper. Final checks confirm helper 1.1.0, the original daemon PID 85147, eight running containers, User trust present, and Admin trust absent. The production CA fingerprint remains `A0:79:59:99:96:EE:67:C6:6E:21:EE:F1:66:42:DC:74:58:ED:EB:91:48:8D:6A:C8:64:F0:01:40:2F:BE:04:BD`. No ArcBox certificate remains in the System Keychain after test cleanup, and `tls/ca-key.pem` remains absent.

The supported follow-up is a native authorization action in the user's GUI session, plus strict trust checks that skip mutations when trust is already correct. The helper can retain public-certificate storage and inspection. First-time trust changes still require macOS authorization; helper installation cannot guarantee that this prompt is eliminated.

## GUI authorization implementation and validation

[ADR 0006](../adr/0006-authorize-local-ca-in-user-session.md) supersedes the root-authorization assumption. The helper now stores and inspects public certificates only. Desktop launches the bundled CLI in the login session when the daemon publishes `AWAITING_HTTPS_TRUST`. Cancellation returns CLI status 130 and leaves explicit retry available on the existing daemon. Migration removes legacy User trust even when the current certificate is unchanged; readiness requires its absence and precise Admin TLS trust.

The login-session probe reported UID 501 and graphics access. The native `arcbox-local-ca` test `trust::tests::native_admin_trust_lifecycle_preserves_other_roots` granted Admin trust to two isolated roots, verified the precise policies, skipped a duplicate grant, and removed the first root while preserving the second. The final revoke returned cancellation `-60006`. The test's destructor retried interactive cleanup and received cancellation again, causing a double panic and exit 134. This run does not count as a passing lifecycle test. The failed test remains unchanged pending approval to repair its cleanup.

The failed cleanup left an in-memory certificate's Admin record with SHA-1 `03DA51B3167391C2E21CE221FC2DBFD803A16A41`. `security dump-trust-settings -d` omitted that record because the certificate was absent from the keychains. Raw `security trust-settings-export -d` exposed it. A fresh export and an exact issuer, serial, timestamp, and policy check identified the isolated record. A native-authorized `security trust-settings-import -d` removed only that record; a second raw export confirmed that all other records and metadata were unchanged. No production root was changed.

Focused checks pass: 7 common tests, 79 helper tests, 14 CLI tests, and 2 daemon authorization-wait tests. Common, helper, CLI, and daemon Clippy checks pass. Workspace formatting and layer checks pass. Desktop's focused startup suite passes 12 tests, including a daemon restart that needs authorization, cancellation without repeated prompts, and explicit retry. Desktop lint reports no violations. These checks do not establish the installed Desktop-to-CLI path; application deployment and its HTTPS smoke check remain pending.

## Installed Desktop authorization and restart

The corrected runtime and Desktop were deployed together on 2026-10-08. The signed application and helper were backed up before replacement. The helper's root-only `native_storage_lifecycle_preserves_other_roots` test passed. The installer verified that the System Keychain certificate inventory and raw Admin trust export were unchanged after the test, then installed helper 1.2.0.

Desktop automatically launched its bundled `abctl tls trust`; no manual production trust command preceded startup. `WatchSetupStatus` recorded `AWAITING_HTTPS_TRUST`, then `Local HTTPS trust ready`, `VM_STARTING`, and `READY`. At 12:51:10 and 12:51:13 (Asia/Singapore), macOS `authd` recorded successful login-user authentication for `com.apple.trust-settings.user` and `com.apple.trust-settings.admin`. Both authorization records identify `/Applications/ArcBox.app/Contents/MacOS/bin/abctl`, PID 43770.

The installed `abctl tls status` passed. A separate Security.framework query confirmed that current User trust was absent and Admin trust was present. The public CA remained byte-identical to the pre-deployment certificate, and `tls/ca-key.pem` remained absent. A disposable Alpine HTTP server on port 8080 returned the expected body through its actual `https://<unique-name>.arcbox.local/` URL. The check used macOS `/usr/bin/curl` with system trust and no custom CA or insecure flags.

The local Release build initially omitted production service configuration. Packaging was corrected to preserve the original OIDC, Fleet, telemetry, and Sparkle configuration before signing. Deep strict code-signature verification passed, and Desktop's Sign In action was enabled again. The original production application remains available in the rollback backup.

A second normal application launch reached `READY` without entering `AWAITING_HTTPS_TRUST`. Raw Admin trust exports before and after that launch were byte-identical, as was the public CA. A second disposable container with a new hostname passed the same verified HTTPS check, exercising a fresh guest and certificate request. Both disposable containers were removed. The original eight running containers and every saved stopped container were restored to their pre-deployment states.

This validates the installed Desktop-to-CLI authorization path and subsequent startup with existing trust. Live cancellation and retry were not repeated; the focused Desktop tests cover those branches. The earlier native trust lifecycle test remains a failed run pending approval to repair its interactive cleanup. This deployment is a paired local test build; a release must publish the runtime changes and update Desktop's runtime pin together.
