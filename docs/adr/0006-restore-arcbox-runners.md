# ADR 0006: Self-hosted GitHub Actions runners return as ArcBox Runners

- Status: accepted (2026-10-25); supersedes ADR 0004 decisions 2 and 3
- Deciders: ArcBox product owner
- Restored from: `c6d818ad11f07c62c955e8b3653cf1ce3cb8df22` (`archive/platform-before-macos-focus-2026-10-08`)
- Server side: `arcboxlabs/platform` (`arcbox-runner-gateway`, the `provision_runner` workflow, the `/runners` API)

## Context

ADR 0004 removed the Fleet agent so that ArcBox would be a local product only. Teams that own several Macs and Linux hosts across several GitHub organizations still need one backend that enrolls those hosts, routes each queued Actions job to a host that can serve it, and shows the state of every host and job. A host-local runner manager cannot place a job across hosts or organizations, so the agent and its gateway protocol come back.

The name Fleet did not say what the feature does. The feature returns as **ArcBox Runners**.

## Decision

1. ArcBox Runtime and Desktop still operate without a Platform account. Runners is an opt-in feature: a host takes part only after its owner enrolls it.
2. Restore the agent, both protocol crates, their release workflows, and the Desktop runner management from the archive commit. Rename them according to the vocabulary below. Do not keep a Fleet alias.
3. Keep the placement model of the archived Platform: one runner pool per workspace, any number of GitHub organizations bound to one workspace, and each job offered to the online host with the most headroom that serves its `(os, arch)`. The agent stays the admission authority.
4. Keep the `runs-on` labels (`arcbox`, `arcbox-<os>-<arch>`), so existing workflows need no change.
5. ADR 0004 decisions 1 and 4 to 6 stay in force. The bare-Linux node profile (charter P5) stays archived.

### Vocabulary

| Concept | Name |
|---|---|
| Feature | ArcBox Runners |
| Hosts that one workspace enrolls | runner pool |
| One enrolled machine | runner host |
| The per-job GitHub process | Actions runner |
| Agent binary and crates | `arcbox-runner-agent`, `arcbox-runner-proto`, `arcbox-runner-control-proto` |
| Gateway protocol | `arcbox.runner.v1` (`RunnerGatewayService`), BSR `buf.build/arcboxlabs/runner` |
| Local control protocol | `arcbox.runner.control.v1` |
| Local state | `~/.arcbox/runner`, LaunchAgent `com.arcboxlabs.runner.agent`, Keychain service `dev.arcbox.runner-agent`, `ARCBOX_RUNNER_*` |
| Gateway endpoint | `https://runners.arcbox.dev` |
| Release assets | `release.arcboxcdn.com/runner/agent/<version>/arcbox-runner-agent-<os>-<arch>` |

## Consequences

The rename changes the gateway's gRPC service path, metadata key, and ID prefixes, so an agent enrolled under the Fleet name cannot attach to the renamed gateway. Every host must enroll again. Remove the old agent first:

```sh
launchctl bootout "gui/$(id -u)/com.arcboxlabs.fleet.agent"
rm -f ~/Library/LaunchAgents/com.arcboxlabs.fleet.agent.plist
rm -rf ~/.arcbox/fleet
security delete-generic-password -s dev.arcbox.fleet-agent
```

The agent depends on the daemon's Docker socket and `MacosService` only through their public contracts. The local runtime does not depend on the runner crates, and `cargo xtask check-layers` keeps it that way through `Layer::Runner`.

The vendored `runner/arcbox-runner-proto/proto` copy follows the Platform module byte for byte; the root `AGENTS.md` keeps the landing order.

## Alternatives considered

- Keep a host-local runner manager without a backend: it cannot place jobs across hosts or organizations, which is the reason for this decision.
- Restore the code under the Fleet name: keeps a name that does not describe the feature and that users had to learn.
- Rename only user-facing strings: leaves two names for one feature in the code, the protocol, and the database for as long as the feature exists.
