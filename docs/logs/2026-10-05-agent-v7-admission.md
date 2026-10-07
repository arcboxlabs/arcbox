# 2026-10-05 — Every agent business request requires protocol v7

- Type: change
- Area: `arcbox-engine`, `arcbox-transport`, agent protocol
- Commits / PRs: `568344b`, `67aad961`, `46b128dd`, `7a7baf95`, `817fd3b`, `46242e12`, `14ce9343`, `eb23beb`, `f639d02`, `7bf55b11`
- Related: [Protocol compatibility](../../rpc/arcbox-protocol/README.md), [HV poweroff completion](../experiments/2026-10-05-hv-poweroff-completion.md)

## Current status — 2026-10-07

The [published-asset validation](2026-10-07-storage-recovery-release-validation.md) records the subsequent review fixes, resolved Linux failures, and final boot pin. The results below describe the earlier validation stage.

All four approved test corrections are applied: the API fixture creates its directory, the async heartbeat assertion accounts for Tokio's millisecond deadline rounding, the recovery E2E uses `data/docker.img`, and the privileged Guest harness requires four complete test names with one passing execution each.

| Gate | Latest established result |
|---|---|
| Host API and Engine before the stream-consumer correction | Formatting and strict all-target Clippy passed; 195 tests passed, 2 existing tests ignored, 0 failures or filtered tests. |
| API after the stream-consumer correction | Formatting and strict all-target Clippy passed; all 26 unit and 3 icon integration tests passed, with 1 existing ignored test and 0 filtered tests. The new stream lifetime regression passed. |
| Engine async storage-health lifecycle | Commit `774f73f09dc1c86ee57c28249c87cb42ecec972d`; 167 tests passed and 1 doctest ignored. Engine sources remained unchanged during the later API correction. |
| Guest and corrected rootfs | Fresh musl release/test builds, strict all-target Clippy, and four privileged tests passed. Each test executed once; kernel-enforced read-only behavior and cleanup passed. [Exact inputs](../experiments/2026-10-05-storage-recovery-io-fault.md#current-guest-validation-on-2026-10-07). |
| Healthy recovery and Desktop integration | Corrected-rootfs VZ recovery and two real Desktop XCTest recovery actions passed, including protected replay after restart. |
| Healthy HV recovery | Passed with `gic`, no `vmnet`, no NFS mount, and an audited reused Guest. This result does not cover default-feature HV. [Exact scope](../experiments/2026-10-05-storage-recovery-io-fault.md#continuation--healthy-hv-recovery-without-vmnet-on-2026-10-07). |
| Complete storage fault E2E | VZ and HV each passed the unchanged test with a fresh Guest and corrected rootfs. The daemon used `gic` without `vmnet`; the test retained its isolated NFS behavior. [Exact scope](../experiments/2026-10-05-storage-recovery-io-fault.md#complete-storage-fault-e2e-on-2026-10-07). |

The initial Host record is `/private/tmp/arcbox-host-api-engine-validation.nl2xe9k0/validation.json`, SHA-256 `a0b30ba63e5dd83de867b37d7bf8a020df82c4c168522400beeccdbb57699a40`. The final API record is `/private/tmp/arcbox-api-streaming-validation.HIUF4e/fixed/validation.json`, SHA-256 `931a166f908edc939de1efd1fc8bd93ed45bf16d31d735afc86644f92550299b`. Source comparison permits reuse of the Engine result; the later API result includes the added regression.

The corruption-and-missing-disk E2E passed on both backends and is committed as `d03dd36f`. API admission is committed as `160c1a8d` and `7594e58d`; the latter includes the independent input progress and response-drop cancellation regression. Default-feature HV recovery remains unverified. The two full Guest-suite failures reproduced on baseline `1106545` remain unresolved. The full DAX probe failed with both the changed Host and baseline Host `1106545e`. Both runs used the same Guest artifact, kernel, rootfs, and configuration. No baseline Guest was tested, and the DAX cause remains undetermined. Production publication and pin changes are outside these validation results.

The sections below preserve historical source identities and the state of each earlier run. Their pending-approval statements describe those earlier stages.

## Problem or trigger

The storage contract requires protocol v7 for observations and mutations. Checking the protocol only during boot left direct RPC and stream entry points able to contact a v6 agent. The synchronous VM shutdown path also bypassed `AgentClient` and accepted any response bytes. A cancelled or failed Ping could leave an incomplete frame on a reusable connection.

## What was done

The current and minimum agent protocol versions are both 7. Shared unary RPC entry points check protocol admission before sending a business frame. Direct readiness, stats, memory, sandbox, and machine exec/debug/TCP entry points use the same check.

A compatible Ping admits only the current connection. Disconnect clears admission. An incomplete, cancelled, malformed, or failed Ping closes the connection. Ping can return a complete incompatible response for diagnosis, but that response does not admit business requests. The previous-version rejection regression remains unchanged.

Synchronous VM shutdown uses `AgentClient`, including protocol admission, typed error handling, and the guest's `accepted` field. Ping and Shutdown each use the existing five-second RPC deadline. The guest grace period and subsequent VM-stop wait remain unchanged. The HV probe completes admission before measuring guest poweroff.

The storage-health client uses the same admission check. Dropping its stream cancels the receiver and closes the connection. An owned shutdown handle interrupts a blocking receive or handshake when cancellation occurs.

The storage-check client also requires admission on its dedicated connection. Cancellation closes that connection. The response preserves failed check results, and callers must inspect `passed` before releasing protection.

System VM start, reboot, and reserved recovery start inspect the staged agent before VM execution. The agent must contain the storage-protection capability marker because an old agent can mount disks before the Ping handshake. The marker identifies supported behavior; it does not authenticate the binary. Missing and old agents leave the machine in its original state without assigning a CID.

The guest dispatches offline checks only in the isolated recovery mode and write verification only in the normal System VM. Unknown actions return 400; actions from the wrong guest mode return 403. Each check uses a dedicated connection, echoes the trace identifier, and closes the connection after its response. Offline checkers have bounded output and a deadline. Online verification requires durable volume writes, a successful local Docker container, and successful removal of the owned container and image.

## Evidence

Each candidate used a separate Cargo target directory. Every staged source entry matched the candidate that passed validation.

| Candidate | Check | Result |
|---|---|---|
| Transport close | `cargo test --locked -p arcbox-transport --lib` | 26 passed |
| Unary admission and Ping cleanup | `cargo test --locked -p arcbox-engine --lib` | 143 passed |
| Direct stream admission | `cargo test --locked -p arcbox-engine --lib` | 144 passed |
| Shutdown acceptance | `cargo test --locked -p arcbox-engine --lib` | 147 passed |
| Storage-health client | `cargo test --locked -p arcbox-engine --lib` | 151 passed |
| Storage-health cancellation transport | `cargo test --locked -p arcbox-transport --lib` | 26 passed |
| Storage-check client | `cargo test --locked -p arcbox-engine --lib` | 157 passed |
| Staged-agent admission | `cargo test --locked -p arcbox-engine --lib` | 158 passed |
| Staged-agent daemon integration | Existing `backend_matrix` E2E | VZ and HV passed; both daemons exited with status 0 |
| Guest offline checking | Linux musl Clippy, selected Linux tests, normal daemon matrix | 30 test executions passed; VZ and HV passed; both daemons exited with status 0 |
| Guest online verification | Linux musl Clippy, selected Linux tests, normal daemon matrix | 34 test executions passed; VZ and HV passed; both daemons exited with status 0 |

Socket regressions send v6 and v7 Pong frames through the real transport. The regressions verify that v6 receives no business frame, v7 admission is reused on one connection, and partial-Pong cancellation closes the socket. Shutdown regressions cover acceptance, guest errors, and unexpected response types.

Formatting and host Clippy with `-D warnings` passed for the transport, admission, and shutdown candidates. The shutdown candidate also passed E2E all-target Clippy and Linux Engine all-target compilation. Host Clippy reported no first-party warning. Cargo reported an existing future-compatibility advisory for the locked third-party `proc-macro-error2 v2.0.1`. Linux compilation reported ten dependency warnings in six files; those files match baseline `e5e9cca` byte-for-byte.

The storage-health candidate passed workspace formatting, Engine/Transport all-target host Clippy, and Engine/Transport Linux musl all-target compilation. Its Linux warnings come from the same unchanged dependency files. All 1,447 staged source entries matched the validated snapshot. These health regressions use blocking transport fixtures, including the async API test; they do not establish coverage of the actual async transport.

The storage-check candidate passed the same formatting, host Clippy, and Linux compilation checks for Engine. All 1,448 staged source entries matched the validated snapshot. Its real macOS async transport tests cover v6 refusal, v7 check results, guest errors, unexpected response types, handshake and receive cancellation, and request expiry. Each terminal path checks peer EOF. These socket tests do not execute a guest checker.

The signed debug `hv_e2e` binary passed the existing boot-only probe with copied development assets and no writable data disk. The new client admitted the guest protocol and required shutdown acceptance. The guest emitted PSCI SYSTEM_OFF before host teardown, and all probe assertions passed. The probe used `ARCBOX_HV_E2E_BOOT_ONLY=1`, two vCPUs, and 1 GiB of guest memory. This check establishes boot and shutdown behavior; it is not a performance benchmark. The complete DAX probe was not repeated.

The staged-agent candidate also passed formatting, host Engine Clippy, Linux Engine compilation, and fresh release builds of the daemon, CLI, and normal guest. Its complete 1,448-entry staged tree matched the validated source. The existing daemon matrix reached READY, checked NFS, and exercised image pull and container create, run, logs, exec, stop, and removal on both VZ and HV. The matrix records results before teardown, so both daemon exit statuses were checked separately. All four staged guest copies matched the fresh build SHA-256 `f189b6dcb973f45c7223b11bf47a5b9c694eb533e047e4047e9bc09c7d8995d2`. Developer ID verification and both virtualization entitlements passed before and after the matrix. This matrix does not run storage recovery or the full DAX probe.

The matrix used the candidate's own `target/release` binaries and the development 0.8.8 asset bundle. Run the existing driver from that source with `ARCBOX_PROFILE=development ARCBOX_BOOT_ASSET_VERSION=0.8.8 SKIP_BUILD=1 KEEP_TEST_DIR=1 RUST_LOG=info cargo test --locked --target-dir target -p arcbox-e2e --test backend_matrix -- --ignored --nocapture`. Build the native release daemon and CLI and the `aarch64-unknown-linux-musl` release agent first. Do not substitute a Rust test executable for the normal guest.

The two guest checker candidates passed workspace formatting and Linux musl all-target Clippy with `-D warnings`. Each candidate built a fresh normal agent and its own matrix harness. The host binaries and 13 assets matched the audited staged-agent source and were copied unchanged. The fresh offline agent SHA-256 was `f18146d048c0a1bff6d2bcc241cdd67ba00037eacb7e7c18fb41c29c57373457`; the online agent was `32ef7b9235a96e6c11ae148b37f25d6b1d59a511b20fbc8163db95c49d21fe7e`. Both matrices verified the staged guest copies, signed daemon identity, harness identity, and normal daemon exits. The 1,449-entry offline tree and 1,450-entry online tree matched their validated snapshots before commit. The selected tests and normal matrices do not establish Core recovery orchestration or clear the historical full-suite failures.

## Continuation — storage ownership and application admission

Engine commit `e62d3d48ae74e4c69821516a0e89de365339981f` serializes storage maintenance and joins reserved VM stops. Its exact candidate passed formatting, 164 Engine library tests, strict host Clippy, Linux all-target compilation, and the normal VZ/HV daemon matrix. Both daemons exited with status 0. The matrix proves normal startup and Docker lifecycle behavior; it does not exercise Core recovery orchestration.

Core commit `4fa67d4abf99bca9794d503323ba28c07f109c31` reconciles the durable recovery journal before boot. Its 336-line candidate passed formatting, 131 Core library tests, strict host Clippy, and Linux all-target compilation. The staged source matched the validated candidate. Linux compilation retained warnings in unchanged dependencies and two deprecated aliases in an unchanged Core example.

The application source reviewed at that stage retained the Engine reservation when durable protection failed. Daemon shutdown propagated failure. Core, Connect, Docker, Computer, and SSH shared write admission. Existing Connect sessions rechecked stdin, resize, and file-upload chunks, including the final empty commit. Existing SSH sessions rejected later input after protection started. Earlier queued input was not retroactively revoked.

| Source | Check | Result |
|---|---|---|
| Complete application tree `b04983c7b64125cb96778717abacf4d980cba74a` | Workspace formatting; seven-package host all-target Clippy; dependency layers | Passed |
| Same application tree | Core, daemon, CLI, Docker, Computer, and SSH package tests | 568 passed, 0 failed, 27 existing ignored, 0 filtered; API excluded |
| Startup-and-observation candidate `dc4e2a7b9e4337964da556a95db09e23c2854d14` | Formatting; Core library tests; host all-target Clippy; Linux musl all-target compilation | Passed; 148 Core tests, 0 failed, ignored, or filtered |

Run the application package tests with `cargo test --locked -p arcbox-core -p arcbox-daemon -p arcbox-cli -p arcbox-docker -p arcbox-computer -p arcbox-ssh`. The application source manifest contains 1,473 entries and has SHA-256 `7beaf6b98dd9d1f952ee6ca4fbea4bce6b4281b8157e43b08e4e721ebf153426`. The 397-line startup candidate contains 1,461 entries; its manifest SHA-256 is `1eac913fae9f0fa48ac6b81dd080303d359b8dc96ac83718b141a923de714f9c`. The startup candidate includes the `CoreError::from` conversion required after reserving storage. Both manifests remained unchanged during their checks. These later checks reused one canonical Cargo target directory; the earlier separate-target statement applies to the preceding candidates. Host Clippy retained only the existing third-party future-compatibility advisory. Startup Linux warnings matched the candidate baseline.

The seven-package gate was incomplete at that stage. API reported 22 passing tests and three cleanup failures because its fixture did not create a temporary directory; the unchanged elevated retry failed identically. Resize and empty-commit assertions had not executed. The fixture correction awaited approval, and missing target OpenSSL and `pkg-config` configuration blocked the application musl check. Later results supersede those blockers.

The user approved the 1,199-line recovery-action candidate and its exception to the 400-line commit limit. Commit `70bc45d3641730f48bd8d24e5d7c9dd573cd7944` records that candidate. Its first Core run failed the existing listener assertion; the unchanged elevated retry passed all 143 tests. The cause of that intermittent failure remains unresolved. Later passing Core results establish their own source results and do not prove a fix for the listener failure.

The protocol audit found no Host business request or observation path that admits a v6 Guest. Ping can report an incompatible Guest version without admitting the connection. This check is directional: the Ping request does not carry a Host protocol version for reverse admission by the Guest. The original previous-version rejection test remains unchanged.

The four privileged Guest checks had passed separately in Lima, as recorded in the [storage I/O experiment](../experiments/2026-10-05-storage-recovery-io-fault.md). The heartbeat timer, recovery E2E image-path, harness test-count, and API fixture corrections were still awaiting approval during that run. No production pin changed.

## Continuation — current application matrix and Lima prerequisites

The normal VZ/HV matrix passed on application tree `b430c58d3f2990894f220460a008053c8c5ad407`. Its 1,473-entry source manifest has SHA-256 `33da01f9502897888e3f85e429eaa963be3453e93b0106a85596d30c8a47ee27`. The daemon, CLI, and matrix harness were built from that source. The signed daemon SHA-256 was `fdfc41be6ff28802e0d7075f79b8b1aea3224784c1f520789cb143cc2393be7d`.

Both backends completed readiness, NFS export, image pull, container creation, foreground and background execution, logs, exec, and container removal. Both daemons exited with status 0. The audit verified the actual harness, signed daemon identity, both staged Guest copies, and all 13 assets for each backend. The reused Guest had SHA-256 `32ef7b9235a96e6c11ae148b37f25d6b1d59a511b20fbc8163db95c49d21fe7e`. Its production logic matches the current source; the recorded differences contain only test code, a test script, and documentation. This was not a fresh Guest build.

The matrix record is `/private/tmp/arcbox-independent-normal-matrix.iNJkNf4x/independent-normal-final/combined-validation.json`, SHA-256 `afaba8f82356cb6e115addba5e240509a26c8003a47c972284f75fe122a00259`. Independent verification checked 60 referenced artifacts and both shutdown lines in the raw matrix output. This matrix established normal lifecycle behavior; it did not establish API, Linux compilation, recovery E2E, or DAX results.

The same source was copied to Lima `arcbox-m2` for native GNU compilation. Both attempts stopped before compilation because the offline Cargo cache lacked `rcgen`. Dependency synchronization awaited approval at that time. The failure record is `/private/tmp/arcbox-lima-native-gnu.uxdof9v2/combined-validation.json`, SHA-256 `7701303af748a3c7ecc187ab982bd9d9d9f32b18b94e5f3e75a63e1d1bf027e4`. Subsequent Colima validation supplied the missing dependencies.

The Desktop storage feature passed its six-suite regression gate after the termination-observation fix: 39 tests, zero failures, and zero skips. All 501 source-file hashes remained unchanged. Four generated light/dark captures passed visual inspection for clipping, overlap, and layout. The record is `/tmp/arcbox-desktop-storage-consolidated.GV10iREm/validation.json`, SHA-256 `799a99d9bdde95f10fd6dd66f0f7c0e0c908fbde36895099652309e40299d8c2`. These selected tests and static captures do not establish full Desktop-to-Runtime recovery integration.

## Continuation — Colima validation on 2026-10-07

The user approved installing the missing validation dependencies. Isolated Colima build images supplied GNU and musl toolchains, OpenSSL development files, standard protobuf includes, and rustfmt. Cargo fetched the locked dependencies. The active Docker context remained `arcbox`; commands addressed the Colima socket explicitly.

Linux compilation exposed two daemon startup dependencies on macOS-only code. Commit `090f1b9306f7d42b49845a1b1ed0e751a90a21cb` makes the existing `libproc` dependency available on Linux, restricts `FileResolver` setup to macOS, and reads the fixed DNS environment keys directly. The change preserves macOS behavior. Workspace formatting, strict host daemon Clippy, and all 83 host daemon tests passed. The first sandboxed test run had permission failures; the unchanged elevated retry passed. The Linux fix did not change `Cargo.lock`.

Both native targets passed the seven-package all-target compilation check and the same seven focused test invocations:

| Target | All-target compilation | Focused tests |
|---|---|---|
| `aarch64-unknown-linux-gnu` | Passed | 32 passed, 0 failed, 0 ignored |
| `aarch64-unknown-linux-musl` | Passed | 32 passed, 0 failed, 0 ignored |

The 32 tests comprise 21 Core recovery tests, one API storage-status test, three Docker admission tests, four daemon shutdown tests, and one test each for CLI interruption, Computer resume admission, and existing SSH sessions. Every invocation executed at least one test. These focused tests do not replace the full API or workspace test gates.

The check command was `cargo check --locked -p arcbox-core -p arcbox-api -p arcbox-daemon -p arcbox-cli -p arcbox-docker -p arcbox-computer -p arcbox-ssh --all-targets --target TARGET`. The exact test commands, dependency images, initial failures, and final logs are recorded in `/private/tmp/arcbox-native-seven-matrix.icbld9jt/combined-validation.json`, SHA-256 `22a1a10954c53ae5875ad98568d377dc54d9152bfb80d3393da473ff3925c620`. Both validation source copies and the working source matched the 1,473-entry manifest with SHA-256 `e9cb850a126ba8f9f14921c72b496bcf2021b68e19e1889849a49d62518abc30` before this documentation update. The final comparison caught an unrelated workspace dependency removed by `cargo remove`; restoring the original `arcbox-container` declaration made the working source match the tested source. Generated protocol sources also matched their original hashes.

Both corrected rootfs architectures passed the clean and corrupted ext4 regression. The [storage experiment continuation](../experiments/2026-10-05-storage-recovery-io-fault.md#continuation--corrected-rootfs-and-live-recovery-on-2026-10-07) records the checker defect, artifact identities, and live recovery results. The VZ diagnostic passed check-only, daemon restart with protection retained, explicit recovery, durable writes, and Docker verification. Both daemon incarnations exited with status 0.

Desktop then executed `RuntimeStorageRecoveryIntegrationTests.testRecoveryAgainstIsolatedRuntime` twice against another disposable VZ runtime. The real `RuntimeStorageRecoveryModel`, `ArcBoxClient`, and `DaemonManager` completed check-only and recovery. The second invocation first observed the previous protected operation after daemon restart. Both invocations passed without skipping. The watcher agreed with each result and the VM state. The test also verified that Desktop retains write blocking after check-only and releases write blocking after recovery.

The Desktop record is `/private/tmp/arcbox-desktop-recovery.9rxelhve/audit.json`, SHA-256 `ca604071b8fa138992b73ecc71404a5b6ef47eee74abe9dfa22ba52c971ff273`. All 503 inventoried source and configuration files remained unchanged during the live run. The preparatory build separately passed nine model tests and skipped the opt-in live test because no socket was supplied. That skip is not counted as a live execution. Formatting, project generation, and lint passed. The only new test source is `arcbox-desktop/ArcBoxTests/RuntimeStorageRecoveryIntegrationTests.swift`; its opt-in environment contract is documented in that repository's `docs/development.md`.

The live runs reused the previously audited signed host binaries and Guest. They used the corrected ARM64 rootfs, disabled the VZ bridge NIC and NFS mounting, and used disposable data and socket paths. These results establish healthy recovery and restart replay through the Desktop model's real RPC integration. They do not establish GUI interaction or the full fault-recovery scenario. HV was not run at that stage because the VZ diagnostic flag does not disable HV's bridge NIC; the later HV run used a separate build without `vmnet`. No production asset or Desktop runtime pin changed.
