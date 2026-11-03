# 2026-11-02 — A Mac without hardware virtualization boots the System VM on QEMU TCG

- Type: change
- Area: `arcbox-vmm` (`darwin_qemu`, `darwin_net`, `VmBackend`), `arcbox-engine` (`vm_lifecycle`, machine reboot), `arcbox-core` (`qemu`, config, runtime), `arcbox-daemon` (startup reaping), `arcbox-api`, `abctl`
- Commits / PRs: this change
- Related: [no-virtualization host](2026-11-02-no-virtualization-host.md), [daemon lifecycle](../daemon-lifecycle.md) "QEMU left by a crashed daemon", probes in [`tests/bench/qemu-tcg/`](../../tests/bench/qemu-tcg/README.md)

## Problem or trigger

A Mac without hardware virtualization (a macOS VM, such as an Amp Mac
satellite) cannot run VZ or HV, so since the no-virtualization fix its
daemon runs as a Machine host with no System VM. The plan is to boot the
same System VM there under QEMU's TCG emulator, with dockerd inside and the
guest unchanged.

## What was done

- `VmBackend::Qemu` ("qemu"). `VmBackend::on_host` is the fallback rule: on
  macOS without hardware virtualization every configured backend boots as
  QEMU. Callers boot with the result and never record it, and a VZ or HV
  start error never triggers it. `Runtime::effective_system_vm_backend`
  reports it (`effective_backend` = QEMU); `SetSystemVmBackend(QEMU)` is
  accepted.
- `virt/arcbox-vmm/src/vmm/darwin_qemu/`: the command line as a pure
  function (`args.rs`), the child process (`process.rs`: `posix_spawn` with
  `POSIX_SPAWN_CLOEXEC_DEFAULT`, its own process group, a pidfile under
  `run/qemu/`), QMP (`qmp.rs`: `SHUTDOWN` with `guest-shutdown` completes a
  graceful stop, `guest-reset` flags the in-place reboot, an unrequested
  `STOP` is logged), and stop (`quit`, then SIGTERM, then SIGKILL, 5 s
  apart). Snapshots return `Unsupported`.
- Devices: NIC1 is the VZ frame socketpair (`darwin_net::GuestNic`, shared
  with VZ) on `-netdev dgram,local.type=fd`; no NIC2, vmnet or Rosetta.
  `hvc0`/`hvc1` are two `virtconsole` ports whose socketpairs feed the
  engine's serial drain, which `MachineManager::reboot` now re-attaches
  because each QEMU process has new ones. The three disks are
  `virtio-blk-pci` at fixed slots 2–4, `format=raw`, `discard=unmap` on the
  writable ones. vhost-user fs and vsock are not attached yet; the
  `QemuMachine::initialize` comment marks where their socketpairs go.
- The daemon stops QEMUs a crashed predecessor left (`reap_orphaned_qemu`,
  before the `docker.img` holder scan). The pidfile's `F_GETLK` holder is
  what gets signalled, never the pid written in a possibly stale file.
- `arcbox_core::qemu::binary` finds the emulator: `[qemu] path` or
  `ARCBOX_QEMU_PATH`, else an error naming both.
- Startup timeouts (agent wait, dockerd readiness) are multiplied by
  `VmBackend::startup_timeout_factor`, 2 for QEMU. Without explicit
  `[vm] memory_mb`, the System VM gets at most 4096 MiB where it boots on
  QEMU.

## Evidence

All on an Amp Mac satellite (`mac.large`, M4 Pro, 12 vCPUs, 28 GiB,
macos-26, `kern.hv_support` absent) with arcboxlabs/qemu
`v0.0.0-dev.0169f2b` (QEMU 10.2.4), boot assets 0.8.12, through the
production `Vmm` (`tests/bench/qemu-tcg/run.sh`).

Devices, network and lifecycle (`run.sh` sections 1–4):

| Check | Result |
|---|---|
| Block order | `vda` 16352 sectors, `ro=1`, EROFS magic `e2e1f5e0`, PCI `00:02.0`; `vdb` 64 GiB at `00:03.0`; `vdc` 2 GiB at `00:04.0` |
| Discard to the sparse file | `docker.img` 131072 blocks after the guest wrote 64 MiB, 0 after `blkdiscard` |
| Network | DHCP lease 10.0.2.2/24, router and DNS 10.0.2.1, from the daemon's stack; `example.com` resolves; HTTP fetch works |
| Consoles | `hvc0` lines and an `hvc1` line reach the drain |
| Guest power-off | QMP `guest-shutdown`, `wait_for_stopped` true, QEMU exit 0, pidfile gone |
| Guest reset | QMP `guest-reset`, `reset_requested=true`, `Vmm::reboot` in 31–35 ms, the second boot's `hvc0` drained |
| Host stop | QMP `quit` path, 7–37 ms, pidfile gone |
| Orphan | after `kill -9` of the owner QEMU kept running (ppid 1, own group, pidfile held); `reap_orphaned_qemu` stopped it in 70 ms and removed the pidfile |

Through the daemon (`arcbox-daemon --data-dir … --foreground`):
`GetSetupStatus` reports `effectiveBackend: SYSTEM_VM_BACKEND_QEMU` and
`hardwareVirtualizationAvailable: false` while `GetSystemVmBackend` still
returns VZ. A QEMU orphaned by the probe was stopped at the next daemon
start ("Stopped QEMU processes left by a previous daemon", `reaped=1`).
The real rootfs reaches `/sbin/init` at 1.2 s; rcS then fails on the missing
virtio-fs share `arcbox` and the guest powers off.

Speed: `guest/bench.c`, the same source natively and under TCG, seconds:

| | integer loop | memcpy 20×64 MiB | `getppid` ×1M | `fork`+`wait` ×500 |
|---|---|---|---|---|
| native macOS | 0.275 | 0.022 | 0.094 | 0.137 |
| TCG, 4 vCPUs | 0.304 | 1.575 | 1.188 | 7.429 |
| TCG, 12 vCPUs | 0.309 | 1.570 | 1.259 | 7.858 |

dockerd (static binaries from runtime 0.8.12, overlay2 on tmpfs, no
iptables) from exec to `API listen on`, 4 vCPUs: 1.74 s at 2 GiB, 1.73 s
at 4 GiB, 1.30 s at 8 GiB, 1.75 s at 14 GiB; kernel to `/init` 1.46–1.50 s
at every size; QEMU spawn to dockerd ready 3.3–3.9 s. At 12 vCPUs dockerd
took 2.0 s against 1.3 s at 4.

The hardware budgets are 90 s (boot to agent) and 150 s (agent to
dockerd); QEMU spawn to dockerd ready takes under 4 s in the probe.
Factor 2 keeps a loaded host and the 13–55× syscall/fork slowdown inside
the budget, and bounds how long a failed boot holds the daemon: a SIGTERM
during the boot waits for it (below), which took exactly 360 s at the
earlier factor of 4.

Memory: host free memory (`vm_stat`) fell 0.9 GiB at boot, then 3.0 GiB
while the guest filled a 3 GiB tmpfs, and stayed there after the guest
deleted it; it came back only when QEMU exited. QEMU's `phys_footprint`
stayed at 260 MB throughout: guest RAM is a shared memory object, charged
to no process. Guest size changed neither boot nor dockerd time, so the
TCG default only caps what the guest can hold on to.

## Follow-ups

- The agent starts from the `arcbox` virtio-fs share, so the System VM
  reaches neither the agent nor dockerd until the vhost-user fs and vsock
  back-ends are attached. Executing ELF files from that share also needs
  the patched QEMU that reports DIC=0 (its `-cpu` property goes in
  `args::CPU`).
- `wait_for_agent` does not notice a guest that powered off, and the
  daemon's shutdown joins System startup before stopping the Runtime, so a
  SIGTERM during a failed boot waits out the whole budget (180 s on QEMU).
  Same shape on VZ and HV with 90 s.
- The guest's own dockerd poll budget is fixed inside the guest.
- `ARCBOX_VM_MEMORY_MB` is split by figment into `vm.memory.mb` and never
  reaches `[vm] memory_mb`; only the config file sets it.
- `abctl uninstall` does not stop a QEMU left by a crashed daemon.
