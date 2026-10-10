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
