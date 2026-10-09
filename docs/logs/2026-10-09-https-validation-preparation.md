# 2026-10-09 — HTTPS packaging prerequisites and isolated NAT validation are ready

- Type: change
- Area: local HTTPS, Desktop packaging, guest NAT validation
- Source revisions: runtime `49b51a85ba4084e8c311e44297020f3930f1502b`; Desktop `a214a210bc875c1ad3b13456dcb85bea276bc28e`
- Related: [acceptance matrix](../local-https.md#completion-requirements), [isolated NAT driver](../../tests/bench/local-https/README.md)

## Problem or trigger

The development profile's Compose cache did not match `assets.lock`. The available Linux validation image lacked the legacy iptables tools needed to check real kernel rules. Both prerequisites were prepared after the user authorized their installation.

## What was done

The existing `abctl --profile development --format json docker setup` path refreshed the development tool cache. Compose is now `v5.6.0`; the binary and checksum sidecar match the locked ARM64 SHA-256 `bd714a42b46e51757fc1121085b3096b9064e3f80d3ca5a991e33f44df4f43c9`.

Desktop's existing signed DMG command built the paired app from the revisions above. The build used `--dev`, `--skip-resources`, and the local runtime checkout, with the Xcode embed phase enabled. `ARCBOX_HOST_TARGET_DIR` and `ARCBOX_HOST_BIN_DIR` selected that checkout's `target` and `target/release`. `DAEMON_SIGN_IDENTITY` and `--sign` selected Developer ID `34B370D02E5D7D81DFD7739FA0BC0C25AE948ACC`.

The validation Dockerfile extends the existing local musl image without replacing its tag. It installs pinned `iptables` and `iptables-legacy` `1.8.13-r0`, and `iproute2` `7.0.0-r0`. `/sbin/iptables`, `iptables-save`, and `iptables-restore` select the legacy backend. The [driver](../../tests/bench/local-https/README.md) records the build and execution commands.

## Evidence

| Check | Result |
|---|---|
| Paired App | `ArcBox Dev`, version `1.37.1`, build `627`; Swift build succeeded |
| Deep strict signature verification | Passed with Developer ID team `422ACSY6Y5`, including the nested daemon and host tools |
| Stable signing identifiers | CLI `com.arcboxlabs.desktop.cli`; helper `com.arcboxlabs.desktop.helper`; daemon `com.arcboxlabs.desktop.dev.daemon` |
| Embedded CLI versions | `abctl 0.8.2`; `arcbox-helper 1.2.0`; Compose `v5.6.0` |
| Guest provenance | Embedded `arcbox-agent` and `vm-agent` exactly match fresh stripped copies of the checkout's release binaries |
| DMG integrity | `hdiutil verify` passed; SHA-256 `68f65aa2f36f5e4f4f7af40fd2a6f353a541f02ae9b4f3dae2262e5278eb9d6a` |
| Derived Linux image | `arcbox-validation-musl-net:1.96.0-20261009`, ID `sha256:bf15a93cde9f8a4b9888f02357cce62043efdc708bcac24312f4cbd864a18f32` |
| Base Linux image | Unchanged ID `sha256:766c0dfb539f26ef7c15251a25a6874a658103301b7ec2aea53c9b4adf78a0e7` |
| Legacy tool operations | Insert, check, list, delete, save, restore, and exact baseline restoration passed on Linux `6.8.0-117-generic` |
| Production publisher regression | `kernel_index_repairs_order_and_preserves_unrelated_rules`: 1 passed; stale-port replacement, repeated publication, damaged ordering repair, repair-path duplicate cleanup, DNS withdrawal, and unrelated-rule preservation |

The App is at `target/dmg-build/ArcBox Dev.app` and the DMG is at `target/ArcBox-Dev-1.37.1-arm64.dmg` in the runtime checkout. The guest protocol and minimum remain `7/7`; helper remains `1.2.0`. This commit adds validation and documentation only, so the packaged production implementation remains current.

The isolated containers used `--network none`, `--cap-drop ALL`, explicit `NET_ADMIN` and `NET_RAW`, and `no-new-privileges`. They had no host mounts and were removed after execution. A `NET_ADMIN`-only attempt failed before any rule mutation: the legacy backend also needs a raw control socket. This capability requirement was verified by the successful run, not inferred from tool installation.

The new test runs the production `Publisher` and `TaggedRules` against actual iptables output. Ordinary repeated publication creates no duplicate redirects. The repair path removes a deliberately injected duplicate after Docker's jump moves above the redirects. Correctly ordered external duplicates are not claimed to trigger repair.

## Follow-ups

The App has not been installed or launched. Production ArcBox and a separate CPU reproduction daemon both had active VMs during the readiness audit. The [E2E guidance](../../tests/e2e/README.md#parallel-daemon-control-planes-one-data-dir-per-daemon) prohibits another VM harness in that state. Normal App launch also starts its runtime. A native acceptance window must first coordinate those workloads.

The `local_https` harness header now states that startup can reconcile host routes through the helper. Its isolated data directory does not isolate host routes. Its guest-port requests do not authorize macOS CA trust and do not prove the Desktop browser prompt.

Packet forwarding, production-kernel behavior, installation, browser authorization and cancellation, and the remaining acceptance-matrix rows are unverified. The optional `pstramp` terminal helper was absent during packaging; Docker terminal behavior was not checked. No release or notarization was performed. The previously canceled trusted-root cleanup remains excluded.

## Machine route kernel validation

The additional `kernel_machine_discovery_replaces_routes_and_cleans_up` regression passed in a fresh container from the same validation image: 1 passed, 448 filtered out. It exercises `EndpointFacts::machine`, actual `/proc` listeners, Host-checked HTTP discovery, and production `Routes` and `TaggedRules` operations.

The test covers repeated scans, listener disappearance and rediscovery, changing the HTTP backend while its previous listener remains open, managed-to-native port selection, both protocols disabled, explicit removal, and shutdown. Shutdown must release its bound frontend. Unrelated NAT rules, the complete original NAT table, and loopback addresses remain byte-for-byte equal to their respective baselines. The whole test has a 30-second deadline. The [driver](../../tests/bench/local-https/README.md) records the exact test name and isolation requirements.

The frontend is a held socket, and the explicit native TLS port has no server. This result proves rule management and discovery, not TLS or packet forwarding. The machine configuration receiver still needs a session-owned route actor and an authenticated public-root source. The old actor must finish cleanup before a new session can claim the same address.

Source inspection also found a boot prerequisite: the boot shim moves `/arcbox` and copies BusyBox into a distro root, but does not retain the boot root's static `/sbin/iptables`. The routing code currently uses that absolute path. Product-owned tool delivery must be connected before enabling routes across supported distros; an arbitrary distro package is not a guaranteed substitute.
