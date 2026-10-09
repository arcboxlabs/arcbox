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

The App is at `target/dmg-build/ArcBox Dev.app` and the DMG is at `target/ArcBox-Dev-1.37.1-arm64.dmg` in the runtime checkout. The guest protocol and minimum remain `7/7`; helper remains `1.2.0`. These packaging checks apply to the source revisions recorded above. Later production changes require a new paired build.

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

## Machine public-root delivery

Machine web replacements now carry complete policies and the current public CA, or explicit absence. The host observes policy and CA changes independently, records the acknowledged snapshot, and resamples both sources. Root changes during Open, during Ack, and while idle reuse the retained connection on both host transports.

The guest validates the public root and both policies before changing accepted state. Invalid replacements preserve policies, root identity, and published routes. Disconnect clears proxy authority before awaited stream shutdown; session admission remains occupied until shutdown finishes. This channel does not change machine client trust.

| Check | Result |
|---|---|
| Host machine web tests | 3 passed, including both host transports |
| Linux guest machine web tests | 8 passed in the isolated Linux validation image |
| Transport tests | 29 passed; protocol and gRPC test commands also passed |
| Engine machine web test | 1 passed |
| Daemon control-plane tests | 5 passed |
| Host and Linux guest all-target Clippy | Passed with `-D warnings`; the dependency future-incompatibility notice for `proc-macro-error2` remains |
| Workspace formatting | `cargo fmt --check` passed |
| Incremental protocol check | `buf breaking` against the preceding `a8ad4544` source passed |

The full `buf breaking` comparison against `origin/master` still reports the existing `MachineService.Import` to `ImportMachine` rename. The unchanged preceding source produces the same finding. No compatibility RPC, protocol bump, or gate suppression was added.

Machine frontend consumption of the session root, route activation, signing listeners, and boot tool delivery remain pending. These results establish authenticated root delivery and withdrawal, not machine HTTPS. The signed App and DMG above predate this production change and must be rebuilt before native acceptance.

## Machine network tool ownership

Boot-assets commit `6a6d971` installs the static boot tool at `/usr/lib/arcbox/iptables` before user mounts and distro processes start. The helper rejects symbolic path components and insecure ancestor ownership or permissions. Installation uses a temporary file in the destination directory, assigns root ownership and executable permissions, and atomically replaces the previous file. A missing source or failed copy preserves the installed tool. The distro's own iptables remains unchanged.

Every user mount now validates its destination before directory creation or mount. The helper rejects dot components, symbolic links, and paths that cover or enter the product directory. Invalid shares retain the existing console-report-and-skip behavior. The machine shim stops if tool installation fails.

Boot-assets formatting, strict Clippy, CLI build, 15 unit tests, and the local release-asset E2E passed. The live CDN and QEMU tests remained ignored. The actual helper passed behavior checks with Alpine 3.19 BusyBox in a disposable container with no network or host mounts and only `CHOWN` capability. The checks cover repeated replacement, old file descriptor contents, absolute execution, ownership, source-copy failure cleanup, distro-tool preservation, and mount path rejection. These checks do not build or boot the new EROFS image. The boot release and runtime asset pin remain unchanged.

Guest NAT owners now retain the selected executable for check, insert, delete, list, and sweep operations. System owners continue to select `/sbin/iptables`. Machine route activation must select the new product path after boot delivery is available; no PATH lookup or distro-tool fallback was added.

The fresh Linux guest test binary passed 95 domain tests, three iptables tests, and four publish-mirror tests in a container without extra capabilities. The two opt-in kernel tests each passed in separate loopback-only containers with `NET_ADMIN` and `NET_RAW`. The machine regression records calls through its selected executable separately during installation, removal, and shutdown. Temporarily removing that executable makes shutdown fail and leaves the NAT table unchanged; restoring it permits cleanup. The index regression still preserves unrelated rules and repairs ordering. Linux all-target Clippy with `-D warnings` and workspace formatting passed; only the existing dependency future-incompatibility notice remains.

These kernel checks exercise the selected tool and cleanup through production rule objects. They do not activate a machine session, bind its signing listener, or verify a boot image. The next machine slice must connect those consumers before the new tool path can serve installed machine HTTPS.

## Machine connection cancellation

Machine configuration now transfers the real stream to an owned task. Canceling the RPC caller synchronously withdraws DNS ownership, the public root, and installed publication. The owned task completes stream shutdown before the last session owner releases admission. A replacement racing with withdrawal returns a closed-session error instead of panicking.

Eight machine receiver tests and the host/guest admission regression passed in the isolated Linux container. The stream wrapper blocks real shutdown while the test cancels the caller during reading or shutdown. Both cases withdraw authority immediately and reject a new session until shutdown finishes. Linux all-target Clippy with `-D warnings` and workspace formatting passed. These checks cover connection ownership; the machine route actor, frontend activation, and installed acceptance remain pending.

## Machine frontend and session CA integration

The receiver now starts a route actor with the product-owned iptables path. Open waits for a strict initial rule sweep and both frontend/signing listener binds before publishing the private claim. Replacements update the in-memory public root and await tracking replacement before Ack. Both actor cleanup and real stream shutdown retain session admission. Machine frontends use only the session root; System frontends retain their published-file source.

The fresh Linux binary passed 96 domain tests, eight machine receiver tests, and the host/guest admission regression. The new production-source/cache regression rejects a withdrawn root immediately despite a cached leaf and an indefinitely pending signer. Republishing the same root permits reuse of that still-valid leaf. Both isolated kernel regressions passed in separate containers. Linux all-target Clippy with `-D warnings` and workspace formatting passed. Receiver tests inject a route worker; the isolated kernel checks do not use AF_VSOCK. Successful machine signing, the new boot image, packet forwarding, and native acceptance remain unverified.

## Machine initialization and cleanup ordering

The new lifecycle regression directly polls the production session protocol over an in-memory connection. Open cannot publish or respond before frontend readiness. A replacement cannot acknowledge before tracking completes. Cancellation closes the pending update reply and produces no late acknowledgment. Initialization cancellation retains admission until the worker exits; the test checks withdrawal and the complete empty output before opening another session.

The owned-connection tests now block stream shutdown and route cleanup independently. Both completion orders retain admission until both operations finish. A separate production `Routes::run_machine` regression forces the initial sweep to fail through the selected executable. Readiness fails, cleanup still attempts another sweep, and both failures reach the caller.

The fresh musl binary passed ten machine receiver tests and the production initialization-failure test in the isolated Colima image, without network, host mounts, or extra capabilities. Linux all-target Clippy with `-D warnings` and workspace formatting passed. These checks establish protocol and cleanup ordering. Successful AF_VSOCK binding, signing, packet forwarding, and installed machine HTTPS remain unverified.

## Shared private-key initialization barrier

Runtime initialization previously returned from VM-host-only mode before removing the legacy shared `tls/ca-key.pem`. A regression test reproduced the remaining key with System VM autostart disabled. Initialization now removes the key before either the autostart branch or the storage-recovery hold. Background CA preparation cannot substitute for this required barrier because preparation errors do not block runtime readiness.

Two macOS regression tests pass: initialization removes only the legacy key while preserving the public certificate and an unrelated application key; a real removal failure stops initialization before runtime assets or VM milestones. The existing no-System-VM milestone test also passes. Core all-target Clippy with `-D warnings` and workspace formatting pass. The existing `proc-macro-error2` future-incompatibility notice remains. These tests use temporary directories and do not create a Keychain identity, alter native trust, or start a VM. Independent machine startup and installed migration acceptance remain pending.

## DNS upstream request ownership

A real loopback UDP regression reproduced DNS service completion while an upstream request still held the listener. The service now keeps the listener in its main future and retains upstream blocking tasks in a `JoinSet`. Cancellation stops receiving and replying, cancels queued jobs, and joins running queries. Listener errors and task panics are reported after cleanup; ordinary upstream errors still produce SERVFAIL with the forwarding cause logged.

Nine DNS tests pass, including an upstream request held across cancellation, immediate port reuse after orderly completion or forced service drop, successful upstream delivery, invalid transaction ID rejection with SERVFAIL, local resolution, and NODATA. The cancellation test directly polls the service after the upstream receives the query, without timing-based readiness assumptions. Daemon all-target Clippy with `-D warnings` and workspace formatting pass. The existing dependency future-incompatibility notice remains. These checks use temporary loopback sockets and do not start a VM or change resolver files. The daemon's outer five-second drain and startup task retention still require integration; forced service drop releases the listener but does not interrupt running blocking I/O.

## DNS ownership through daemon exit

DNS now uses the shared `BackgroundTask` retained by startup and daemon contexts. Normal shutdown, interrupted startup, and failed startup join the service before stopping Runtime. DNS is outside the five-second server abort budget. An interrupted join preserves the service for another cleanup waiter. The task reports DNS failures when they occur and retains the original result for cleanup. Control-plane binding moved into `services/control_plane.rs`; gRPC still starts before DNS binds.

Two new regressions first reproduced both startup exits returning while a real loopback upstream held a query. Both now pass. Each test cancels its first cleanup waiter, drops the original startup handles, and resumes cleanup through a clone. The tests verify the held listener, subsequent port reuse, released NetworkManager, and absence of late replies. The fixture has no Runtime or container-network lease.

All 29 targeted daemon tests pass: nine DNS, six shutdown, nine service, and five control-plane tests. Daemon all-target Clippy with `-D warnings`, workspace formatting, and independent review pass. The existing `proc-macro-error2` future-incompatibility notice remains. Normal daemon exit was checked in source; no VM, native trust, resolver, or installed App acceptance ran. Retained RPC operations and complete control-plane shutdown remain prerequisites for independent machine startup.

## Guest socket scope

`vsock::SocketScope` closes admission and interrupts live dedicated sockets on both transports. Transport leases retain duplicate descriptors across split streams; the scope stores only weak references. Normal connection release still delivers EOF while the scope remains open. Registration and closure share one lock, so a connection that finishes after closure is rejected.

All 33 transport tests pass, including real socket checks for idle async receivers, blocking readers, saturated blocking writers, normal EOF, explicit close, and rejected late registration. Transport all-target Clippy with `-D warnings` and workspace formatting pass. One existing documentation test remains ignored. The checks run on macOS Unix sockets; native AF_VSOCK and VM teardown remain unverified. Scope closure interrupts socket I/O but does not join native connectors or channel producers.

## Operational agent connection cancellation

`MachineManager` now registers normal and expected-run agent clients in one socket scope. Final host shutdown can reject new operational connections and interrupt pending guest I/O. The final VM shutdown request keeps its independent connection. Callers must still retain and join connection and operation workers.

Both engine cancellation tests pass. The new test performs the real Ping handshake and sends a one-hour `SandboxExecWaitRequest` through each transport. The peer withholds its response; scope closure causes a receive error within two seconds and releases the socket. The existing test verifies that cancellation joins a blocking worker after socket shutdown. Engine all-target Clippy with `-D warnings` and workspace formatting pass. These socket checks do not start a VM or prove daemon cleanup ordering.

## Built machine boot image

The ARM64 rootfs build at boot-assets `6a6d971` completed successfully. The image has 6,041,600 bytes and SHA-256 `0612c68dd16b42ef4b59339c8df9be879a5dc88b24376217ccd5fb24bde4e2b0`. Boot-assets now includes `tests/rootfs-machine-tools.sh` and its rerun command in the README. The probe passed against this image in a disposable Colima ARM64 Alpine container with read-only source and image access.

The probe extracts EROFS and compares the embedded machine init and helper scripts with source. The extracted helper installs the image's actual static iptables into a temporary distro tree. Installed bytes match, owner and mode are `0:0:755`, and absolute execution reports `iptables v1.8.13 (legacy)`. This evidence establishes image contents and tool installation, not boot, AF_VSOCK signing, packet forwarding, or client trust. The boot release and runtime asset pin remain unchanged.

## Machine readiness after host cancellation

A new regression reproduced readiness retrying after the operational socket scope had permanently closed. The readiness loop now checks admission before acquiring another host hold or connecting. Scope closure ends the loop with its original error instead of waiting for the 60-second readiness deadline.

All six targeted readiness tests pass, including the new cancellation case, stale physical-run rejection, distro initialization, and VM lifecycle ordering. Engine all-target Clippy with `-D warnings` and formatting pass. The new fixture uses a modeled VM; it verifies released host holds and unchanged addresses without starting a guest. Native startup acceptance remains pending.

## Cancellation-aware API preparation

Connect request contexts now accept a daemon cancellation token. Icon lookup and macOS image resolution cancel their read-only HTTP requests. Machine import checks cancellation before preparation and before starting the native import worker; accepted native work remains awaited. Calls without a daemon token preserve their existing behavior.

Both targeted API tests pass: an already-cancelled request never polls its operation, and a real partial HTTP response is disconnected during macOS image resolution. The original direct-call error remains intact. API all-target Clippy with `-D warnings` and workspace formatting pass through devenv. Daemon token delivery and retained handler shutdown are separate integration work; these tests do not start a VM or modify native trust.

## Migration preparation cancellation

Preparation passes one cancellation token to both source and target Docker runners. Cancellation closes output pipes, kills and reaps the direct child, and rejects subsequent commands. Closing pipes prevents a descendant that inherited stdout or stderr from retaining the preparation handler. Command failures still preserve stderr. Migration execution remains a separate owner.

All 38 migration crate tests and 12 Core migration tests pass. The new real-process checks cover source and target cancellation, inherited output pipes, child reaping, later-command rejection, an empty prepared-plan registry, and stderr preservation. Fixtures clean up their own descendants. Strict all-target Clippy and workspace formatting pass through devenv; the two cancellation tests also pass after the native-pointer lint correction. These checks do not run a real migration or start a VM.

## Machine image cancellation and file cleanup

Machine image pulls now accept cooperative cancellation for metadata resolution, lock waits, and HTTP requests. File creation, reads, writes, and flushes remain awaited. Failed downloads drain pending writes before the staging guard removes files. A final cancellation check prevents publication before the synchronous commit. Machine Create passes the request token and rechecks cancellation before native mutation. Boot preparation retains its existing bounded download lifecycle.

All five image tests pass, including a real partial HTTP download with Tokio's only blocking worker held before its file write. The progress callback cancels before another network read; the pull remains pending until the file worker is released. Completion leaves no staging or registry entry, and a subsequent pull succeeds through the same manager. Image, Core, and API all-target Clippy and workspace formatting pass through devenv. These checks do not boot the downloaded image or validate native HTTPS.

## Retained RPC handlers through daemon cleanup

The daemon now closes RPC admission, cancels preparation, interrupts guest sockets, and joins admitted handlers before Runtime teardown. Normal exit first unmounts NFS exports. The handler owns its completion lease, so client cancellation cannot discard native work or its subsequent asynchronous cleanup. Input cancellation reports `Unavailable` before EOF. The force-shutdown prompt now appears when the runtime stop starts listening for another signal.

The partial-input and caller-drop regressions pass. A real blocking worker and a separately held cleanup future keep the barrier pending until both finish; closing between admission and task creation also retains the handler. Daemon all-target Clippy, workspace formatting, and independent source review pass. No new handler-level wait cycle was found. Native callbacks and local file operations remain awaited; response producers and detached workers require separate ownership. These checks do not prove complete control-plane teardown or installed VM shutdown.
