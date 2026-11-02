# 2026-11-02 — The daemon raises its own open-file limit, and HV vsock descriptors are no longer capped at 64

- Type: change
- Area: `arcbox-daemon` (`fd_limit`), `arcbox-vmm` (`darwin_hv/vsock.rs`)
- Commits / PRs: this change
- Related: [daemon lifecycle](../daemon-lifecycle.md) "Startup Sequence", [HV backend](../architecture/hv-backend.md) "Vsock Protocol"

## Problem or trigger

The daemon never raised `RLIMIT_NOFILE`. launchd gives a job the soft
limit of `launchctl limit maxfiles`, 256 on a stock Mac. The Desktop's
LaunchDaemon plist (`arcbox-desktop`,
`LaunchDaemons/com.arcboxlabs.desktop.daemon.plist`) and the
`abctl _install` agent set no `SoftResourceLimits`, and a daemon started
from a shell inherits the shell's limit, also 256. Rust's std does not raise
it either: a `main` built with rustc 1.99.1 reads soft=256 under a 256
shell.

256 descriptors is the daemon's whole budget. Every Docker client holds a
host Unix socket, and on the System VM every streaming request (`logs -f`,
`attach`, `exec`, events) also holds a vsock stream to guest dockerd for as
long as it runs. A master daemon started by launchd at soft 256 held exactly
256 descriptors (246 accepted Unix sockets) with 1000 idle clients on its
gRPC socket. The other 754 waited in the listen backlog, and the daemon
logged nothing.

On HV a raise alone would not have fixed it. `HvMachine::duplicate_client_vsock_fd`
moves the daemon end of every vsock socketpair to the descriptor numbered by
its host port, a counter that starts at 50 000 and never wraps. `4cf3ec27`
clamped that to soft − 64 when the soft limit is lower, so every connection
past the clamp competes for the same 64 descriptors below the limit:

- Under a limit below about 50 064, a process holds at most 64 concurrent
  vsock connections, whatever the limit is. That includes 10 240 or 61 440,
  which a raise reaches on a Mac whose `kern.maxfilesperproc` is that value.
- Above that, the first (limit − 50 064) connections of the daemon's life
  each get their own number, so the kernel's descriptor table grows past
  50 000 entries. After that the clamp takes over.
- Inside the clamped window, a number that was just closed comes straight
  back. That is the recycling the dup was added to prevent (`198e2e7a`).

The unit test that covered the dup asserted fd ≥ 512, which a 256 soft limit
cannot give (the clamp lands at 192). It failed on master under
`ulimit -n 256`.

## What was done

- `fd_limit::raise` in `arcbox-daemon` runs in `main` before the tokio
  runtime starts. It raises the soft limit to min(hard,
  `kern.maxfilesperproc`), and to the hard limit on Linux. It never lowers
  the limit. The result is logged as `open-file soft limit from=… to=…`, and
  a failure only logs a warning.
  - The clamp is required, not cosmetic. On macOS 26.3.1, `setrlimit`
    accepts and reads back `RLIM_INFINITY` and `maxfilesperproc + 1`, yet
    `F_DUPFD` at `maxfilesperproc` fails with `EINVAL` even with a soft
    limit of `RLIM_INFINITY`. setrlimit(2) documents `EINVAL` for those
    values. With the clamp the call is valid on both behaviours and the
    stored limit is the real ceiling. `OPEN_MAX` (10 240) is used only when
    the sysctl cannot be read: it is not a kernel cap, because a soft limit
    of 10 241 is accepted and descriptors open up to
    `maxfilesperproc − 1`.
  - The raise lives in the daemon rather than in the plists because one
    change in the binary that needs it covers the Desktop LaunchDaemon, the
    `abctl _install` agent, the e2e harness and a hand-started daemon. A
    plist value would also have to stay below each Mac's
    `kern.maxfilesperproc`.
  - Child processes (`mount_nfs`, `umount`, `mount`, `launchctl`) inherit
    the raised soft limit. None of them uses `select(2)`.
- `duplicate_client_vsock_fd` now takes the host port and starts its search
  at floor + port mod rotation, with floor = min(1024, limit / 2) and
  rotation = min(1024, limit − floor). On `EMFILE` or `EINVAL` it retries
  from the floor, then from 0. The position is a preference, not a cap:
  vsock connections are bounded only by the process limit. Under any limit
  of at least 2048, descriptors stay near 1024–2047 instead of following the
  port number. A closed number returns only after 1024 further connections,
  or when the band is full.
- `test_duplicate_client_vsock_fd_rotates_numbers_without_capping_connections`
  replaces the old test. It checks four things: the socketpair still
  carries data, the original fd is closed, the next connection does not get
  the number just closed, and 100 connections are held at once.
  `fd_limit::tests` checks the raise against `sysctl -n kern.maxfilesperproc`.

## Evidence

All measurements are from an Amp Mac satellite: macOS 26.3.1 arm64, 14 GB,
uid 501, no hardware virtualization. The image sets
`kern.maxfiles = kern.maxfilesperproc = 524288` and `launchctl limit
maxfiles` to 524288/524288. Shells there start with soft 256 and hard
unlimited.

Placement, from `tests/bench/nofile-limit/probe.c`, holding up to 200
connections whose host ports start at port0 (old = the pre-change body,
new = this change):

| Soft limit | port0 | Old | New |
|---|---|---|---|
| 256 | 50 000 | 64, fds 192–255, reused the fd just closed, then `EMFILE` | 200 |
| 2560 | 50 000 | 64, fds 2496–2559, reused, then `EMFILE` | 200, fds 1024–2047 |
| 10 240 | 50 000 | 64, fds 10176–10239, reused, then `EMFILE` | 200, fds 1024–2047 |
| 61 440 | 50 000 | 200, fds 50001–50200 | 200, fds 1024–2047 |
| 61 440 | 61 400 | 64, fds 61376–61439, reused, then `EMFILE` | 200, fds 1024–2047 |
| 524 288 | 50 000 | 200, fds 50001–50200 | 200, fds 1024–2047 |

The HV path cannot run on this host, so the real function is exercised by
the `arcbox-vmm` unit test. Under `ulimit -n` 256, 2560, 10 240 and 524 288
the new test passes. With the pre-change body it fails at 256, 2560 and
10 240 (`a just-closed vsock fd number was handed out again`: 192, 2496,
10 176) and passes at 524 288. The whole `arcbox-vmm` lib suite passes at
256 (126 tests).

Daemon, from `tests/bench/nofile-limit/launchd-limit.sh <daemon> 1000 256`.
The script starts the daemon from a LaunchAgent whose plist sets only
`SoftResourceLimits` to 256 (the stock `launchctl limit maxfiles` soft
value), then holds 1000 idle connections on the gRPC socket:

| Daemon | Log | fds while held | Unix sockets accepted |
|---|---|---|---|
| master (`33663b98`) | none | 256 | 246 |
| this change | `from=256 to=524288` | 1014 | 1004 |
| this change, `kern.maxfilesperproc=61440` | `from=256 to=61440` | 1014 | 1004 |

`launchctl limit maxfiles S H` is not a way to model a stock Mac: it also
sets `kern.maxfilesperproc = S` and `kern.maxfiles = H` (measured 256 and
2147483647 after `256 unlimited`). Under it the daemon raised 256 → 256,
which is the clamp respecting the kernel ceiling. The satellite's settings
were restored afterwards.

Not measured: a stock Mac's `kern.maxfilesperproc` (the satellite's value
is set by its image), and an HV VM serving more than 64 concurrent Docker
streams, which this host cannot boot.

## Follow-ups

- The master daemon's gRPC listener stopped accepting at the limit and
  logged nothing. A daemon that reaches its raised ceiling would be equally
  silent.
- The daemon-level HV e2e ladder that a VMM change gets (`app/AGENTS.md`
  "Validation ladder") has not been run: the Mac this was measured on has no
  hardware virtualization, and CI runs no HV daemon-level scenario. Run
  `cargo test -p arcbox-e2e --test hv_vmm -- --ignored`, then
  `--test boot_assets` and `--test virtio_debug` (each with `-- --ignored`)
  under `ARCBOX_VM_BACKEND=hv` on a Mac that can boot HV. `boot_assets`
  reaches the boot-time probe connections `connect_vsock_hv` retries, which
  is where the anti-recycling placement matters and which the unit test
  covers only for the descriptor numbers, not tokio's kqueue registrations.
