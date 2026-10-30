# 2026-10-10 — Signed HTTPS candidates pass artifact verification

- Type: change
- Area: CLI resource preparation, Desktop packaging, local HTTPS acceptance
- Commits / PRs: Runtime `f3f34331`; Desktop `af3b9e7`, `22f58af`; boot source `6a6d971` with packaged-tool checks at `21bd845`
- Related: [completion requirements](../local-https.md#completion-requirements), [earlier validation](2026-10-09-https-validation-preparation.md)

## Problem or trigger

Independent host startup and Machine HTTPS needed a matching signed candidate with the new boot image. Packaging must select an isolated resource directory rather than populate the installed profile. The previous temporary candidate directory and final CLI validation log were absent when preparation resumed.

## What was done

`abctl boot --data-dir PATH` now selects the boot cache and runtime resources before constructing the boot policy. The real CLI regression verifies that clearing the selected directory preserves the configured directory and that list operations read the correct directory. Formatting, strict CLI all-target Clippy, nine boot tests, the output-format test, and the directory-isolation test pass through `devenv shell`.

Candidate preparation used a detached, independent clone of Runtime `f3f34331`. Only that clone's boot version and manifest hash changed. The real Runtime pin remains `0.8.9`, and Desktop's published Runtime pin remains `v0.8.0`. No protocol version changed.

The retained rootfs matched its recorded hash. The release tool regenerated the candidate manifest using that image and the matching kernel. Every guest runtime file matched the manifest before copying. Docker tools were freshly downloaded through `abctl docker setup` into the isolated resource root. The Docker lock hash validates its downloaded archive; the extracted binary has a separately recorded hash.

Desktop's Makefile built the host Runtime, both guest agents, and separate Development and Production DMGs. A copied Swift module cache retained its old absolute path; removing only that candidate cache allowed the unchanged build command to finish. Production included its matching provisioning profile. Development received no provisioning profile.

## Evidence

Candidate files and logs are retained at `/Users/Xuan/Developer/arcboxlabs/https-candidate-20261010`. The folder's `README.md` describes sources, hashes, verification commands, and limits. `runtime-source.json` and `runtime-candidate.patch` record the exact source deviation.

| Artifact | SHA-256 |
|---|---|
| Development DMG | `070d339431e523afb83551c17fcf200d003939b1a65d02180111809798b709a0` |
| Production DMG | `a0eaa8bca48f99ef2cf641d20a58289236be70d22fc4359e73033df5d6e6e037` |
| Candidate manifest | `73ee49b3d65d7151f96f308b8eedda78da1c0a7c49b8c053e6aabd42c8350023` |
| Candidate rootfs | `0612c68dd16b42ef4b59339c8df9be879a5dc88b24376217ccd5fb24bde4e2b0` |

The new CLI's `--profile production boot --data-dir PATH status --offline --format json` reports `complete: true`, no reasons, and the exact candidate kernel and rootfs paths.

Both apps pass `codesign --verify --deep --strict`. Independent checks also validate Developer ID Team and code identifiers, selected host and guest binary provenance, compiled and bundled `assets.lock`, manifest resource hashes, LaunchAgent arguments, daemon entitlements, and absence of Nix dynamic-library dependencies. The Production profile's App ID, Team, networking authorization, and expiry match. `hdiutil verify` passes for both DMGs; their read-only mounted App inventories match the separately verified apps. Verification unmounts each DMG after comparison.

## Follow-ups

No candidate app was installed or launched, no native trust changed, and no Production workload stopped. Installed DNS, browser access on 443, first authorization and cancellation, CA lifecycle, and new-image Machine boot/signing/forwarding still require an agreed native acceptance window. Development does not own the canonical resolver by default, so default installed DNS acceptance must use Production.

Kubernetes Service/Pod networking and DNS, broader macOS trust portability, per-service diagnostics, and comparable performance remain incomplete. Publishing requires matched boot and Runtime releases before Desktop's pin update, followed by signed/notarized release verification. Candidate preparation does not complete the feature objective.

## Native installation follow-up

The operator authorized the native acceptance window. All 18 running containers stopped through the selected Docker socket. The original App, helper, public CA, configuration state, and offline APFS disk copies were preserved before installation. The installed Production candidate matched all 140 verified inventory entries.

The normal App installer stalled before replacing the helper. The elevated `abctl` process retained an exited child while its Tokio runtime waited for an event. A native process sample and open-file inventory are preserved under the candidate's `native-acceptance/evidence/` directory. The installed helper still matched the original SHA-256 `cf6e2b002c27920edb4988c2f4dbda47c6e46a9e71b73d77b1f060ebb66fdba1`.

Bundled helper probes now use `std::process::Command::output` on a blocking worker, which waits for and reaps the child directly. Probe exit failures, invalid UTF-8, binary equality checks, and the bounded helper RPC inspection retain their existing behavior. The admitted read-only subprocess finishes even if its async caller is dropped.

All eight preflight regression tests, workspace formatting, and strict CLI all-target Clippy pass. The replacement signed candidate still requires the same native installer test. No native DNS, 443, Machine, or first-use authorization result can be inferred from these checks.

## Replacement candidate native results

Runtime `4ac714f6` contains the probe fix. The second-generation Production DMG has SHA-256 `a3ff49ba35da042ae58ac75476f410745748bc90a6d4a2f5eb2620a61643bb2a`. Desktop's portable host build and signed packaging targets produced this candidate. Its independent verifier passed all 140 inventory entries, signatures, binary provenance, and resource checks. First-generation artifacts remain under `generation-1/`; the Development DMG has not been rebuilt with this fix.

The normal App authorization flow completed helper installation. The installed helper matched the bundled SHA-256 `75051033e5df87f64fb6a7365fc0cbc09d25bdc165b020f6ad8032fc7cf98388`, and the installer exited. `WatchSetupStatus` reported host `READY`, System `DISABLED`, and local HTTPS `TRUSTED`. The Machines list and creation control remained available. This result verifies the installer fix and host admission with System disabled.

After removing the acceptance-only autostart override and restarting the App, System and Docker reached `READY`. The daemon log confirms kernel and rootfs selection from `0.8.9-machine-https.6a6d971`. Five original containers restarted through Docker policy; the acceptance driver stopped them again. No independent System start control was verified in the running App.

`dscacheutil -q host -a name arcbox.local` returned `172.17.0.1`. Native `/usr/bin/curl https://arcbox.local` used port 443, verified the SAN and certificate chain, negotiated HTTP/2, and received 200. The in-app browser opened the local-services index without a certificate warning. These checks used the existing trusted CA. They do not establish first-use authorization, cancellation, or native container forwarding.

The disposable `nginx:alpine` container failed before start. Create returned a CA-injection warning, and start returned 500. Docker's log identified `HEAD /containers/<id>/archive` failures while mounting the container's overlayfs with `index=on,nfs_export=on`: `device or resource busy`. Later individual archive queries returned 200 for the existing bundle and 404 for an absent bundle. The failure therefore needs investigation of repeated archive mount/unmount operations; no retry, delay, or injection bypass was added. The failed container was removed.

The standalone `abctl --profile production tls status` query eventually succeeded after approximately 92 seconds. A sample during the delay showed `SecKeyCopyAttributes` entering `SecCDSAKeyCopyExternalRepresentation` and waiting in Security.framework. The public-only status path currently calls `load_root` when an identity journal exists. Native status latency and private-key access in public-certificate inspection remain open.

The CA publication remained byte-identical to the pre-acceptance backup, with certificate SHA-256 `A9FA7415CCCE29386C14719538195C54C73132A0B8AB9D651C9CBC73970E489C`. No trust migration or revocation was requested. The candidate UI also reported that no sign-in service was configured; this local package does not establish complete production service configuration.

The candidate exited normally. The original App, paired System disk clones, and original default-machine configuration were restored. Candidate disk state and diagnostic log tails were preserved. The temporary autostart configuration was removed. Restoration of the exact original helper requires local sudo authentication; workload restoration remains pending until that step completes. Do not treat the feature or native acceptance as complete.

## Original environment restored

The operator restored the original helper and LaunchDaemon plist. Both hashes match the pre-acceptance baseline. The helper backup carried `com.apple.quarantine`; macOS killed the restored executable despite a valid signature and matching bytes. An identical temporary copy executed after removing that attribute. The operator then removed only the installed helper's quarantine attribute after hash and signature checks. The original App restarted, registered its daemon, and reached `READY` with the original `0.8.8` System image. Its 140-entry inventory matches the original backup.

All 18 original workload names are running. Seventeen retain their original container IDs. The missing service was `manus-iam-30f834613b5d-postgres-1`; its container and network were absent, while its named database volume remained. The project's dependency runner executes `docker compose down` when its log follower exits, which explains the observed shutdown cleanup. The exact cleanup actor was not captured. The project state-path hash, resolved Compose configuration, image, retained volume, and original `127.0.0.1:5433` binding were verified before reconstruction through the explicit ArcBox Docker socket. The replacement ID is `7ee3c5541e9f1cfe105700877a722401f37d2eeffb9607e81f15c0533af79724`. PostgreSQL reports healthy and logs that initialization was skipped because the database already exists. The original ID-set check remains failed; a separate check verifies all original names and the explicit replacement mapping.

The CA publication and certificate fingerprint remain unchanged, and native certificate verification passes. Docker context is `arcbox`; `default` is Running and `ubuntu` is Stopped. Configuration existence matches the baseline. The pre-existing unhealthy `arcbox-nats-1` remains unhealthy. Final evidence is in `native-acceptance/evidence/native-restoration-files.json`, `restored-workloads.json`, and `restored-trust-and-config.json` under the candidate directory. Candidate and original disk backups remain retained. Restoring the environment does not resolve the native CA-injection failure, slow TLS status query, or remaining release acceptance requirements.

## Public certificate inspection follow-up

The public lookup now queries the private key's stored application-label hash with `SecItemCopyMatching`, then finds the matching certificate. It does not construct a signer or call `SecKeyCopyAttributes` on the private key. Queries still reject multiple matches, missing recorded keys or certificates, and a certificate that differs from the journal. Signing and destructive cleanup retain their private-key checks.

The existing native identity matrix gained public inspection for the creator, relocated creator, same-team different identifier, and ad-hoc same identifier. Before the fix, the ad-hoc inspection failed at private-key access. After the fix, all four inspections pass, while both unauthorized signing probes still return native error `-25293`. Exact untrusted identity cleanup passes after both runs. No certificate trust changed.

The production-profile read-only probe disables Keychain interaction and checks the expected fingerprint on five reads. The first read took 4.695 ms; subsequent reads took 0.312291, 0.217333, 0.185291, and 0.182250 ms. All reads returned certificate SHA-256 `A9FA7415CCCE29386C14719538195C54C73132A0B8AB9D651C9CBC73970E489C`. The probe is `native_existing_profile_public_inspection` in `common/arcbox-local-ca/src/keychain/tests/signing_identity.rs`; rerun instructions are in `tests/bench/local-https/README.md`.

Desktop's portable build completed for the host binaries and both guest agents. A standalone CLI signed as `com.arcboxlabs.desktop.cli` has SHA-256 `39bec096151d2f1ada3d889711007d223353e80b1808012753ea9f98d6c75d65`. Its full `tls status` call reached helper RPC and failed with a closed connection in 0.60 seconds against the restored original helper. This is not a successful full-status measurement. The paired candidate helper remains required for that acceptance check; the original installation was not replaced again. These results do not establish locked-Keychain or first-use behavior.

The 36 regular CA tests, five isolated identity recovery tests, four isolated rotation recovery tests, two core status tests, and native identity matrix pass. Workspace formatting and strict all-target Clippy for the affected library and callers pass. The build reports an existing future-incompatibility warning for dependency `proc-macro-error2 2.0.1`.

## Archive failure investigation follow-up

The retained serial log identifies the kernel failure as `upperdir is in-use as upperdir/workdir of another mount`. The guest explicitly enables `index=on,nfs_export=on` to support live container NFS browsing. Disabling those options would remove that existing capability.

Read-only review of Moby's current `containerStatPath` implementation shows that the filesystem view closes before the HEAD response returns. Its view worker closes a completion channel before the worker's OS thread exits and releases its mount namespace. This leaves mount teardown ordering as an investigation target; the reviewed upstream source is not proof of the installed Docker binary's behavior. No response-draining change, retry, delay, NFS change, or CA-injection bypass was added. Native CA injection remains a release blocker.
