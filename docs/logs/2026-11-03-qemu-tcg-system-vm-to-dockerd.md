# 2026-11-03 — A Mac without hardware virtualization runs dockerd in the System VM on QEMU TCG

- Type: change
- Area: `assets.lock` (`[qemu]`), `arcbox-vmm` (`darwin_qemu`), `arcbox-core` (`qemu`), `arcbox-e2e` (`qemu` harness), `arcbox-vhost-user` (System VM probe)
- Commits / PRs: this change
- Related: [QEMU host backend](2026-11-02-qemu-tcg-host-backend.md), [on-demand QEMU](2026-11-02-qemu-on-demand-download.md), [vhost-user vsock](../experiments/2026-11-02-vhost-user-vsock.md), [real agent on vhost-user-fs](../experiments/2026-11-02-vhost-user-fs-real-agent.md), probes in [`tests/bench/qemu-tcg/`](../../tests/bench/qemu-tcg/README.md)

## Problem or trigger

The QEMU backend booted the System VM to rcS, but the agent lives on the
`arcbox` virtio-fs share and the host reaches it over vsock, and neither
device was attached. Executing the agent from that share also ran stale
TCG translations on a stock QEMU.

## What was done

- `QemuMachine::initialize` serves every shared directory with
  `arcbox_vhost_user::fs::FsShare` and vsock with
  `arcbox_virtio_vsock::vhost_user::VhostUserVsock` (CID 3), each on its
  own socketpair; `connect_vsock` dials the guest through it, as on HV.
  Stop ends the back-ends after QEMU.
- `-cpu max,pauth-impdef=on,dic=off` (`darwin_qemu::args::CPU`). Before the
  boot, the daemon runs the binary once with `-machine none -accel tcg
  -cpu <CPU> -S` and a QMP `quit`; a QEMU without the property exits 1
  ("Property 'max-arm-cpu.dic' not found"), and the System VM fails with a
  configuration error that names arcboxlabs/qemu. Measured: 19 ms for the
  patched build, 26 ms for the refusal.
- `arcbox_core::qemu::binary` takes `[qemu] path` (or `ARCBOX_QEMU_PATH`)
  first, then the installed build of the pin in `assets.lock`. An empty pin
  says so and names both settings.
- `assets.lock` pins arcboxlabs/qemu v0.1.0 (QEMU 10.2.4, released
  Developer ID signed and notarized; sha256 `60f84ddd…61eab47`, 5,921,565
  bytes, the same bytes on the CDN and the GitHub release), so a Mac
  without hardware virtualization needs no QEMU setting at all.
- The e2e harness and the System VM probe run the agent straight from the
  share; the tmpfs and devtmpfs copies are gone, and the harness serves its
  share with `FsShare` instead of its own device.

Rejected for the `-cpu` check: `-cpu max,help` fails with "No machine
specified", and `-device max-arm-cpu,help` lists neither `dic` nor
`pauth-impdef` (instance properties); booting without `dic=off` would run
the agent's stale code; matching QEMU's stderr after the real spawn ties
the failure to its wording.

`amd64_runtime_supported` returns true on QEMU when FEX is provisioned, as
on HV, rather than unconditionally: a host without FEX still refuses amd64
(ABX-375).

## Verification

Host: Amp Mac satellite, macos-26, Apple M4 Pro (virtual), 12 vCPU,
28 GiB, `kern.hv_support` absent. QEMU `qemu-darwin-arm64-v0.0.0-dev.aa1c259`
(10.2.4, sha256 `c2640cab…7799e4`). Debug daemon and CLI, release agent,
guest 4096 MiB and 12 vCPUs (the TCG defaults). `acceptance.sh` on a fresh
data directory after `abctl boot prefetch`, QEMU from `ARCBOX_QEMU_PATH`
with no config file:

| Step | Result | Plan target |
|---|---|---|
| Fallback | `Hardware virtualization: unavailable`, `System VM backend: qemu (TCG emulation)` | QEMU |
| Cold start to dockerd, fresh data dir (runtime materialized) | 11.32 s | ≤ 60 s |
| Daemon restart to dockerd, 3 runs | 6.87, 7.05, 7.09 s | ≤ 60 s |
| `docker run --rm alpine echo` warm, 5 runs | 1.03–1.09 s | ≤ 2 s |
| … first run, pulling alpine | 3.45 s | |
| `docker pull python:3.13-slim` | 10.22 s | ≤ 30 s |
| `python3 -c 'print(sum(range(10**6)))'` in it | 4.61 s | |
| `run -d` / `exec` / `logs` | 0.72 / 0.21 / 0.06 s | |
| `--platform linux/amd64 alpine uname -m` | `x86_64`, 3.27 s then 1.26 s | runs |
| Guest agent executable | `/arcbox/bin/arcbox-agent` (the share) | from the share |
| Guest `reboot` to a container running again | 14.14 s; the `--restart=always` container back | recovers |
| SIGTERM to daemon exit, 4 runs | 7.29–7.35 s, no QEMU left | |

`sigterm.sh ready`: dockerd 7.33 s after QEMU's spawn, SIGTERM to exit
7.34 s. `sigterm.sh kill`: the boot fails 0.58 s after spawn ("VM stopped
before its agent became ready"), SIGTERM to exit 0.01 s. The System VM
probe passes every check on this build, exec coherence included
(`buffered=2 direct=2 share-stale=0/16`). A stand-in that refuses `dic`
leaves the System runtime `failed` with the arcboxlabs/qemu message and no
QEMU running. After a `kill -9` of the daemon, the next one logs "stopping
QEMU left by a previous daemon" and reaches dockerd in 7.87 s.

Zero-config run, with the pin: `acceptance.sh` again on a fresh data
directory, with no config file and no `ARCBOX_QEMU_PATH`. The daemon
created `runtime/qemu/0.1.0/` and spawned
`runtime/qemu/0.1.0/bin/qemu-system-aarch64` 0.39 s after it started,
download and signature check included; dockerd was ready at 11.96 s and
every check passed (warm `run` 1.01–1.07 s, `python:3.13-slim` 9.97 s,
amd64 `x86_64`, reboot 13.39 s, restart 6.86 s, SIGTERM 7.31–7.34 s with
no QEMU left). `codesign -dv` on the installed binary: identifier
`com.arcboxlabs.qemu.system-aarch64`, Developer ID Application: ArcBox,
Inc. (422ACSY6Y5), hardened runtime; it carries no quarantine attribute.
`syspolicy_check distribution` reports no stapled ticket, which a bare
Mach-O cannot carry, and an internal XProtect error; notarization was not
checked online.

The startup budgets stay at factor 2 (180 s to the agent, 300 s from the
agent to dockerd) and the TCG memory default at 4096 MiB: the slowest cold
start, 11.32 s in all, is far inside both.

## Follow-ups

- `ensure()` runs only at startup, so `SetSystemVmBackend(QEMU)` on a Mac
  with hardware virtualization and no installed build fails "not
  installed" until `abctl qemu import`.
- A refused QEMU is a configuration error, but the lifecycle still retries
  the start four times before reporting it.
- 5.0 s of every SIGTERM is `drain_servers` timing out ("Server drain timed
  out after 5s"); the guest's own shutdown takes 2.3 s. Which task holds
  the drain, and whether VZ and HV hit it too, is not established.
- No bridge NIC on QEMU, so the container route never installs: every boot
  retries it for 8 s and logs "bridge not found in kernel FDB". Container
  IPs are not routable from the host.
- The daemon logs "FUSE: unsupported opcode Ioctl (39)" during boot and
  "unknown opcode 50" (FUSE_SYNCFS) at shutdown; both are answered ENOSYS,
  as on HV.
- Intel Macs are untested; they get no fallback.
