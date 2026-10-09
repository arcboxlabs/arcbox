# arcbox-cli

Command-line interface for ArcBox.

## Overview

This crate provides a thin command-line interface for ArcBox machine management and local integration helpers. Runtime and Docker API serving are handled by the separate `arcbox-daemon` binary.

## Features

- **Machine Management**: Create and manage Linux VMs
- **Runtime Migration**: Import local workloads from Docker Desktop or OrbStack
- **Daemon Lifecycle**: Start/stop daemon process
- **Docker Context Integration**: Seamless switching between Docker and ArcBox
- **Native Kubernetes Integration**: Manage the ArcBox k3s cluster and bundled `kubectl`
- **Boot Asset & DNS Helpers**: Manage boot cache and resolver setup

## Usage

```bash
# Machine (VM) operations
abctl machine create myvm
abctl machine start myvm
abctl machine list
abctl machine ssh -u alice myvm         # login shell as alice; `exec -u` runs a command as a user
abctl machine default myvm              # `exec`/`ssh` without a name use it
abctl machine resize myvm --cpus 4      # takes effect at the next start
abctl machine web myvm                 # show saved web configuration
abctl machine web myvm --http-port 8080 --https-port off
abctl machine stop myvm
abctl machine clone myvm myvm-2         # stopped machines only; copy-on-write
abctl machine export myvm myvm.tar.zst  # self-contained archive
abctl machine import myvm.tar.zst --name myvm-3

# Runtime migration
abctl migrate from orbstack --dry-run          # inspect the plan, change nothing
abctl migrate from orbstack --dry-run --json   # same plan, machine-readable
abctl migrate from docker-desktop
abctl migrate from orbstack --source-socket ~/.orbstack/run/docker.sock --yes
abctl migrate from orbstack --no-start         # recreate but leave stopped

# Daemon management
abctl daemon start              # Start daemon in background
abctl daemon stop               # Stop daemon
abctl daemon status             # Show host and System runtime readiness
abctl info                      # System info
abctl version                   # Version info

# Local HTTPS trust (macOS login session)
abctl tls status --json          # Inspect the profile's certificate and native trust
abctl tls trust                  # Authorize HTTPS through the macOS prompt
abctl tls rotate                 # Replace the CA and migrate trust through the running daemon
abctl tls repair                 # Restore only the current public certificate file
abctl tls untrust                # Revoke trust while containers keep running

# Docker storage checks and recovery
abctl disk check                # Stop the System VM, preserve disks, check offline, leave stopped
abctl disk recover              # Preserve and check disks, then restart and verify writes

# Docker context integration
abctl docker enable             # Set ArcBox as Docker context
abctl docker disable            # Reset to default context

# Native Kubernetes integration
abctl k8s start                  # Start the ArcBox Kubernetes cluster
abctl k8s enable                 # Install kubectl + add the ArcBox kube context
# Development contexts are not activated globally; select one with kubectl config use-context.
kubectl get nodes

# Run containers through Docker CLI
docker run hello-world
```

Run TLS commands as the logged-in user. `tls rotate` requires a running daemon and resumes any pending rotation. The command authorizes the replacement before revoking the old CA, then asks the daemon to publish the replacement. Cancelling authorization before old-CA revocation preserves the original public certificate. `tls trust`, `tls repair`, and `tls rotate` accept `--expected-fingerprint` to pin the current certificate shown by `tls status`.

`daemon start` returns after the daemon takes process ownership. Use `daemon status` to inspect host services and the System runtime separately. Host readiness permits machine operations; Docker requires the System runtime to report `ready`. System startup failure leaves host services available, and status prints the failure cause.

## Machine web configuration

The machine name `default` is reserved for the daemon-managed System VM. Machine create, start, stop, remove, clone targets, and import targets reject that name. Use `abctl system resources`, `abctl system backend`, and `abctl disk` for System VM configuration and recovery. User machine operations remain available when the System VM is disabled, starting, or failed.

`abctl machine web NAME` shows the saved HTTP and HTTPS backend selection. Set `--http-port` or `--https-port` to `auto`, `off`, or a port from 1 through 65535. Omitted options preserve their values. Both protocols default to `auto`; an explicit HTTPS port selects native TLS. These settings persist across daemon restarts, clones, and archives. `machine inspect` includes the saved `web_ports` object. This command saves configuration only; machine web routing is not yet available.

## Output formats

`--format` is global but validated for the selected command before any command
I/O. Unsupported `json` or `quiet` combinations exit nonzero instead of
falling back to decorated text.

Read-only JSON schemas are available for `doctor`, `top`, `disk usage`,
`boot status`, `boot list`, `setup status`, `machine inspect`,
`sandbox inspect`, and, on macOS, `dns status`.
Disk and boot sizes use raw `*_bytes` fields. For read-only commands, quiet
output is limited to `setup status`, which prints `installed`, `partial`, or
`unknown`.

Status exit codes are health contracts: `doctor`, `dns status`, `boot status`,
and `setup status` exit nonzero when their report is unhealthy or incomplete.
A check that could not be run is reported as `unknown` rather than failed —
`doctor` counts it separately and `setup status` still exits zero, because a
login-shell probe that times out says nothing about the integration itself.
On macOS, `doctor` includes the daemon's cached local CA status without requesting authorization. A CA that needs trust, renewal, or repair appears as `warning`, counted in `summary.warnings`; warnings do not change the exit code. Missing or incomplete daemon status remains `unknown`. This certificate check does not probe DNS, HTTPS routing, or application traffic.
Drift the daemon repairs on its next start is likewise reported as a detail,
not as an incomplete cache: `boot status` says so for a runtime binary missing
its executable bit instead of asking for a re-download.
If Docker is unavailable, `disk usage` keeps host image facts, sets runtime
reclaimable fields to `null`, reports the query error, and exits nonzero.

`disk check` and `disk recover` stop running System VM workloads. Both commands print the operation ID and the directory that contains preserved disks and reports. Recovery restarts the System VM only after offline checks pass, then verifies persistent writes. Neither command forces a read-write remount or formats an existing disk. The daemon owns the operation, so closing the CLI does not cancel the operation. The CLI reports success only after a `COMPLETE` result. A failed or disconnected stream exits nonzero; inspect the storage status in ArcBox before starting another operation.

## Exit status

- `0`: command succeeded.
- `1`: an `abctl` operation or daemon connection failed.
- `2`: command-line arguments were invalid.
- `130`: native authorization was cancelled during a TLS command.
- Remote commands (`machine exec`, `sandbox run` / `exec`, and `claude`) propagate the
  command's exit status; sandbox signal exits use the shell convention `128 + signal`.

## Configuration

Socket path resolution order:
1. `ARCBOX_SOCKET` environment variable
2. The active profile or `ARCBOX_DATA_DIR` layout (`<data-dir>/run/docker.sock`)

The production default is `~/.arcbox/run/docker.sock`. `DOCKER_HOST` is
honored by the Docker CLI, not by ArcBox's direct socket clients.

## License

MIT OR Apache-2.0
