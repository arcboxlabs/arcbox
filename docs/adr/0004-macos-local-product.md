# ADR 0004: ArcBox is a local macOS product

- Status: accepted (2026-10-08)
- Deciders: ArcBox product owner
- Archive: `archive/platform-before-macos-focus-2026-10-08`
- Archive commit: `c6d818ad11f07c62c955e8b3653cf1ce3cb8df22`

## Context

ArcBox Runtime and Desktop compete with OrbStack as local macOS software. The Runtime workspace also contained a Fleet agent that enrolled with a Platform gateway and ran GitHub Actions jobs. The Fleet agent consumed the local Docker and macOS VM APIs; the local runtime did not depend on the Fleet crates.

## Decision

1. ArcBox Runtime and Desktop must operate without a Platform account or cloud control plane.
2. Remove the Fleet agent, both Fleet protocol crates, and their build, release, CI, and dependency entries from the active Runtime tree.
3. Preserve the removed source in Git history at the archive reference and exact commit above. Do not keep a second source tree under a tracked archive directory.
4. Keep Docker, Kubernetes, Linux machines, macOS guests, and Sandbox SDKs as local product capabilities. Keep Linux guest code, Firecracker, TAP networking, and Linux validation because macOS workloads use those components inside the System VM.
5. Keep the local daemon RPC contract and SDK APIs. An explicitly configured remote SDK endpoint remains a user-managed proxy to the daemon, not an ArcBox cloud service.
6. Keep software and image distribution infrastructure. Downloading boot assets, VM images, and releases does not introduce a Platform control-plane dependency.

## Archive and recovery

The archived paths are `fleet/`, `.github/workflows/release-fleet-agent.yml`, `.github/workflows/republish-fleet-agent.yml`, and `xtask/src/commands/release/fleet_asset.rs`. Their workspace entries and integration code remain available in the same archived commit.

Inspect the source without changing the active checkout:

```sh
git show archive/platform-before-macos-focus-2026-10-08:fleet/arcbox-fleet-agent/Cargo.toml
```

Recover the complete pre-removal workspace in a separate checkout:

```sh
git worktree add --detach ../arcbox-platform-archive c6d818ad11f07c62c955e8b3653cf1ce3cb8df22
```

The exact commit is authoritative if a branch reference moves. Restoring Fleet to the active product requires a new product decision and verification of its protocol and release dependencies.

## Consequences

Platform and bare-Linux product expansion in earlier architecture documents is outside the current product scope. The existing local library boundaries and guest requirements still apply. Historical design documents and changelogs remain unchanged.

Source archival does not uninstall an existing Fleet agent, stop its LaunchAgent, revoke its enrollment, or remove credentials. The Fleet data directory (`~/.arcbox/fleet`), LaunchAgent (`com.arcboxlabs.fleet.agent`), and Keychain service (`dev.arcbox.fleet-agent`) remain separate from this source change.

## Alternatives considered

- Keep Fleet behind a feature flag: leaves cloud-specific protocols and release maintenance in the local product workspace.
- Copy Fleet into a tracked archive directory: duplicates the recoverable Git history and leaves an inactive source tree to maintain.
- Remove Linux and sandbox components with Fleet: removes capabilities that local macOS workloads require.
