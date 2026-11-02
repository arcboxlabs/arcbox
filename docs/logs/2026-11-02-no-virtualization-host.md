# 2026-11-02 — A Mac without hardware virtualization no longer crash-loops the daemon

- Type: change
- Area: `arcbox-hypervisor` (`capability`), `arcbox-vmm`, `arcbox-engine` (`vm_lifecycle`), `arcbox-daemon` (System startup), `arcbox-api`, `abctl`, `api.proto`
- Commits / PRs: this change
- Related: [daemon lifecycle](../daemon-lifecycle.md) "Independent readiness"

## Problem or trigger

ArcBox 1.38.5 (daemon v0.10.5) installed on an Amp Mac satellite (macOS
26.3.1, `VirtualMac2,1`) exited on every start and launchd restarted it:
`launchctl print` showed `runs = 933`, `last exit code = 1`. The host is a
macOS VM: `VZVirtualMachine.isSupported` is false, `hv_vm_create` returns
`HV_UNSUPPORTED`, and the `kern.hv_support` sysctl does not exist.

In v0.10.5, `boot_runtime` called `Runtime::init`, which booted the System
VM. The VZ start failed (`platform not supported: Virtualization.framework
not available`), the lifecycle retried it three times (0.5, 1 and 2 s), the
startup pipeline failed, `main` returned an error, and the process exited 1.
The Desktop daemon's LaunchAgent restarts non-zero exits
(`KeepAlive.SuccessfulExit = false`, `ThrottleInterval = 5`).

Master no longer exits there: `39c7f1ce` moved System VM startup into a
retained task after host admission, so a failed System VM leaves host
`READY`. On master the same host still retried the start three times,
logged the failure at error level (a Sentry event), reported
`Failed to initialize System VM: VM error: VM error: VMM error: hypervisor
error: platform not supported: Virtualization.framework not available`, and
downloaded boot assets before failing.

## What was done

- `arcbox_hypervisor::host_virtualization()` reports whether any of the
  platform's hypervisors can create a VM: on macOS
  `VZVirtualMachine.isSupported || kern.hv_support == 1`, cached; on Linux
  `/dev/kvm`. Either framework saying yes counts as capable, so a capable
  Mac is never refused.
- `Vmm::initialize` returns `VmmError::PlatformUnsupported` before touching
  a hypervisor when the host has none. The System VM's start loop
  (`start_with_retries`) returns that error without consulting the recovery
  policy.
- System startup checks the host before asset preparation. On a host
  without virtualization it publishes `system_runtime = FAILED` with the
  reason and logs a warning, not an error. Like any other System failure it
  then waits for a storage recovery to retry: on a Mac the retry fails again
  at the cached probe, while a Linux host can pass it once the KVM module
  loads. The daemon keeps running with host `READY`.
- `SetSystemVmBackend` and `SetSystemVmResources` check the host before
  anything else: before recording the backend, writing `config.toml` or
  resolving boot assets. Machine start needs no such check, since create
  already fetched the image and made the disk, and it fails at
  `Vmm::initialize`. Every RPC answers `PlatformUnsupported` with
  `UNIMPLEMENTED` and the host's reason as the whole message, and
  `abctl machine start` prints it. Not `FAILED_PRECONDITION`: that code
  means a state the client can change (stop the machine, switch HV to VZ
  for sandboxes), and `abctl` renders it as "not in a valid state"; no
  client action gives this host a hypervisor.
- `SetupStatus` gains `hardware_virtualization_available` (presence-tracked)
  and `effective_backend`; `SystemVmBackend` gains `QEMU`, which
  `SetSystemVmBackend` rejects with `UNIMPLEMENTED`. `abctl daemon status`
  prints both new fields.
- `VmBackend` decodes from its `u8` encoding through `TryFrom<u8>`, and
  every `== Hv` / `== Vz` branch is a `match`, so a new backend is a
  compile error at each decision.

## Evidence

All on the Amp Mac satellite above, with development-profile daemons on
isolated data directories. The launchd numbers come from
`tests/bench/no-virtualization-host/launchd-runs.sh`, which installs a
LaunchAgent with the Desktop daemon's restart policy.

| Daemon | launchd `runs` | Last exit | System detail |
|---|---|---|---|
| v0.10.5 (`5f470024`), 60 s | 11 | 1 | daemon exits: `Failed to initialize runtime … Virtualization.framework not available` (10 error-level lines) |
| this change, 120 s | 1 | never exited | `System VM cannot start: this Mac does not provide hardware virtualization (neither Virtualization.framework nor Hypervisor.framework is available, as inside a macOS virtual machine)` (0 error-level lines) |

- System `FAILED` 30 ms after daemon start; no `Retrying VM start` line, no
  error-level line, and no `boot/` directory in the data directory.
- `GetSetupStatus` over Connect JSON returns `phase: READY`,
  `systemRuntime.state: FAILED` with the reason above, and
  `hardwareVirtualizationAvailable: false` (`effectiveBackend` is omitted as
  `UNSPECIFIED`).
- `SetSystemVmBackend(QEMU)` returns `unimplemented`. Before the up-front
  check, `SetSystemVmBackend(VZ)` downloaded 26 MB of boot assets, created
  the System VM disks and failed with `INTERNAL` after one start attempt in
  0.27 s (master: 3.5 s across three retries), and `abctl machine start`
  printed only "Re-run with --debug for details".
- With it, `abctl system backend vz`, `abctl system backend hv` and
  `abctl system resources --cpus 2 --memory 2048` each fail in 0.01 s with
  `unimplemented` (HTTP 501) and the host's reason, shown without
  `--debug`; the data directory has no `boot/` and no `config.toml` was
  written. `abctl machine start` on an alpine 3.24 machine prints
  `Error: Could not start machine 'probe': this Mac does not provide
  hardware virtualization (…)` and the machine stays `Created`.

## Follow-ups

- The QEMU download progress fields are left to the download task, which
  publishes them.
- Machine create on such a host still downloads the boot assets and the
  distro image and creates the data disk, for a machine that cannot start
  until a backend that needs no hardware virtualization (QEMU TCG) exists.
