# Local CA rotation and recovery

## Scope

Root rotation must retain the current identity until the replacement has native trust. The daemon creates private keys. The login-session CLI authorizes the replacement and revokes the exact previous certificate. The daemon then publishes the replacement and removes the retired identity.

The host and guest protocol remains 7/7. These changes add no old-version negotiation or compatibility branch.

## Durable state

The rotation journal records `stable`, `preparing`, `prepared`, and `committing`. Each recorded root contains a Keychain slot and public DER. The two slots separate private keys and avoid ambiguous certificate lookup by public-key hash. The journal retains the previous DER until cleanup finishes.

Only `preparing` may create a candidate key or certificate. A malformed journal must fail without changing the selected identity. A durability error must stop cleanup; recovery must persist the commit record successfully before deleting the previous key.

The initial journal commit validates persistence and rejected state transitions. Keychain integration, CLI orchestration, Desktop actions, and native rotation acceptance are subsequent steps; that commit does not expose a rotation command.

The rotation library now prepares one candidate in the inactive slot, reads a pending pair without mutation, and commits only after the candidate has TLS-only Admin trust and the previous root has no User or Admin trust. Commit retries persist the journal before publishing or deleting native items. A completed retry verifies the selected candidate without allocating another key.

## Validation

The crate tests cover reloads at each journal phase, retention of the previous identity during cleanup, malformed records, duplicate identities, and invalid target slots. Native trust and browser acceptance are not covered by these tests.

## Certificate lifetime and paired code generation

`certificate_state` reports missing, valid, renewal due within 30 days, expired, or not yet valid independently of trust. Unknown means inspection has not completed or failed. Desktop disables ordinary trust for inactive roots and directs a not-yet-valid root to the Mac's date and time. Browser observation stops while the root is inactive and starts a new cycle when the same root becomes valid.

Regenerating the complete paired Swift client exposed `MachineService.Import` as an unescaped Swift keyword. The RPC is named `ImportMachine`, matching its request and response types. The runtime and clients change together. `generate.sh --local` reads `ARCBOX_DIR` so code generation uses the same checkout as the runnable bundle.

## Mutation ordering

The CLI and daemon use the same per-profile file lock for mutations. The CLI holds the lock through native trust changes. Daemon preparation holds the lock through publication and signer activation. Uninstall holds the lock across revocation and identity deletion, then removes the lock directory through its inventory.

## Keychain integration

`tls/identity.json` now selects the current Keychain identity. Missing recorded keys or certificates stop preparation without generating replacements. Untrust enumerates current, pending, retired, and separately published roots. Cleanup retains public DER until exact certificate and key removal succeeds.

macOS persists both members of the generated key pair. Cleanup verifies and removes the public key before the certificate and private key. A missing private key does not prevent finding the certificate through the persistent public key's hash and bytes. Apple ignores the supplied certificate label when `SecItemAdd` receives `kSecValueRef`; identity lookup therefore does not rely on certificate labels. The [Apple implementation](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/SecItem.cpp#L4932) and the isolated native regression agree on this behavior.

## Verification on 2026-10-08

- The affected host library and binary suites passed 464 tests. The final local CA suite passed 22 tests, with six native tests excluded from the default run. Workspace formatting and strict all-target Clippy passed for core, CLI, local CA, daemon, and API.
- All four isolated Keychain regressions passed: journal selection and missing journal, missing private key, missing recorded identity, and interrupted cleanup retaining only a public key or an unfinished key pair. The existing native migration regression also passed. These tests do not install trust.
- Swift generation, formatting, lint, and all three local HTTPS test suites passed. The full Desktop suite failed two existing saved-window-frame tests. AppKit mapped frames from the external display to the current built-in display; the tests expect unchanged coordinates. The tests remain unchanged pending approval.
- Native Admin trust authorization, the browser flow, and a signed build containing these changes remain unverified. Rotation orchestration and the Desktop renewal action are still pending.

The failed early cleanup probe left one isolated public key and certificate. An exact-reference cleanup verified the profile label, public-key hash, full certificate SHA-256, absent private key, and absent User/Admin trust before deleting those two items. The empty temporary directory was removed. No production root or trust setting changed.

## Rotation orchestration and renewal

The CLI now exposes `abctl tls rotate`. Prepare and commit RPCs dispatch to one daemon worker with a bounded queue. The worker holds the profile lock through identity mutation, signer activation, and status publication. Shutdown waits for accepted work. The CLI releases its lock before each daemon RPC and holds the lock through native authorization and revocation.

The CLI compares the daemon's exact previous and candidate DER with the recorded pair before authorizing trust. The expected fingerprint pins the currently visible root, including the candidate after an interrupted commit. A retry reuses the recorded pair. Desktop renews through the CLI, checks the resulting certificate validity and trust, and acknowledges the confirmed root. A valid root retains the renewal action so an interrupted commit remains recoverable.

The following checks passed after connecting the complete command path:

- `devenv shell cargo test -p arcbox-local-ca -p arcbox-api -p arcbox-daemon -p arcbox-cli --lib --bins`: 314 passed; 11 native local CA tests excluded.
- `devenv shell cargo clippy -p arcbox-local-ca -p arcbox-api -p arcbox-daemon -p arcbox-cli --all-targets -- -D warnings` and `devenv shell cargo fmt --all --check`: passed. Cargo reported the existing dependency future-incompatibility notice for `proc-macro-error2 2.0.1`.
- Four isolated Keychain rotation tests passed without installing trust. The tests cover candidate reuse, interrupted preparation, missing prepared identities, and partially deleted previous identities.
- Desktop client generation, `make format`, and `make lint`: passed. The focused `LocalHttpsModelTests`, `LocalHttpsAccessModelTests`, and `LocalHttpsAccessTests` run passed 34 tests. The earlier two full-suite window-frame failures remain unresolved; this run does not establish a passing full Desktop suite.

## Canceled native trust validation

The ignored test `keychain::tests::rotation::native_rotation_rejects_old_trust_and_resumes_publication_and_partial_cleanup` installed TLS-server Admin trust for three isolated roots. Revoking the previous root then returned Security status `-60006` (authorization canceled). The original fixture retried revocation while unwinding, received the same cancellation, and aborted. This test did not pass. Production certificates were not changed.

Read-only inspection confirmed the following residual certificates. Each has one login Keychain public key, certificate, and private key, plus Admin trust. None has User trust or a System Keychain certificate copy.

| Temporary profile | Identity | SHA-256 |
|---|---|---|
| `.tmpUwX1qT` | Previous root | `794612FA08726BB06AD266567BAB688DC3FC1798A8BFE910C90E53782D2B36AC` |
| `.tmpUwX1qT` | Candidate | `C9F08EC76F42AA99D6C654E971B5A9F3C5D500512CDE66F922B629886A83DF78` |
| `.tmp4gk50q` | Independent profile | `6E4522DFBF8CA16757F07B5E892785439195C2ECBF313E9863D72697E8B5735B` |

The revised fixture retains its directory immediately and saves each root's public DER before trust mutation. Its destructor only reports recovery paths and fingerprints. Explicit success cleanup verifies that every retained root is untrusted before deleting identities. A filesystem regression confirms that a panic preserves the journal and public DER. The revised native test has not been retried.

An exact-reference cleanup script passed Swift type checking and read-only inspection. Applying cleanup awaits renewed user authorization because revocation was canceled. The script verifies full fingerprints, key labels and public-key hashes, journal bytes, and a file whitelist. Cancellation stops immediately. Identity deletion starts only after all three roots have no User or Admin trust.

## CA management without a Linux VM

Review found that `--no-linux-vm` skipped CA worker registration, so rotation failed even with a running daemon. The worker now registers independently of Linux VM startup. Docker, guest signing, and browser observation retain their existing VM requirements. The recovery test covers both running and stopped VM states and confirms that CA status changes leave the VM state and startup phase unchanged.

The unused `AWAITING_HTTPS_TRUST` phase and Swift mapping were removed. The paired components use `local_https` for CA status. Protocol version and minimum remain 7/7; no old-client branch or RPC alias was added.

After this correction, the host tests passed 340 tests across local CA, protocol, gRPC, transport, API, daemon, and CLI. Strict all-target Clippy and workspace formatting passed. The three focused Swift HTTPS suites passed 34 tests, and Swift formatting and lint passed. The native trust and full Desktop limitations above still apply.

`buf breaking` against `origin/master` reports only the previously recorded `MachineService.Import` to `ImportMachine` rename. The rename is an intentional paired-component change. The compatibility gate remains failing for that rename; the check was not disabled or filtered. Removing the unused HTTPS phase adds no finding against that base.
