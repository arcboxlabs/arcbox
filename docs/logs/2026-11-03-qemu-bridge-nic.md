# 2026-11-03 — The QEMU System VM gets a vmnet bridge NIC, and the host reaches containers by IP

- Type: change
- Area: `arcbox-vmm` (`darwin_net`, `darwin_qemu`), `arcbox-engine` (`MachineManager::bridge_mac`), `tests/bench/qemu-tcg`
- Commits / PRs: this change
- Related: [QEMU System VM to dockerd](2026-11-03-qemu-tcg-system-vm-to-dockerd.md) (its follow-up "No bridge NIC on QEMU"), probe [`acceptance.sh`](../../tests/bench/qemu-tcg/acceptance.sh)

## Problem or trigger

The QEMU backend attached only NIC1, so the host had no bridge interface to
route the container subnet to. On every start the cold-start route
reconcile looked the VM's bridge MAC up in every bridge's forwarding table
(`ensure_route_with_retry_for_network`, five attempts 2 s apart), logged
"route install failed after all attempts" and "failed to install container
route on cold start", and container IPs stayed unreachable from the Mac.

## What was done

- VZ's vmnet bridge NIC moved into `darwin_net::BridgeNic`: a
  vmnet.framework Shared interface the daemon starts with the VM's stable
  bridge MAC (`bridge_nic_mac`), a `SOCK_DGRAM` socketpair, and
  `VmnetRelay` on the daemon's runtime. VZ attaches it as before.
- QEMU inherits the socketpair's other end as a second
  `-netdev dgram,local.type=fd` behind
  `virtio-net-pci,netdev=net1,mac=<vmnet MAC>,romfile=,addr=0x09`, the slot
  after vsock; the virtio-fs shares now start at `0x0a`. The guest names it
  `eth1` and the agent's `detect_bridge_interface` configures it, so the
  guest is unchanged. QEMU's own `-netdev vmnet-*` was not used: it needs
  `com.apple.vm.networking` or root in the QEMU process, which is signed for
  JIT only.
- `QemuMachine::vmnet_interface_info` feeds `Vmm::vmnet_interface_info`, so
  `MachineBridgeExt::vmnet_bridge_name` finds the bridge and route setup
  takes the "vmnet direct" path, as on VZ and HV.
- When vmnet refuses (a daemon without the entitlement that is not root),
  the machine boots without NIC2 and logs one warning per VM start, "no
  bridge NIC: the host cannot reach container IPs on this run", with
  vmnet's error.
- `MachineManager::bridge_mac` names only a bridge NIC the VM has: on VZ
  always (vmnet's, or VZ NAT's without vmnet, both with the stable MAC), on
  HV and QEMU only the running vmnet interface's. Without one, route setup
  skips at once instead of searching 8 s for a bridge that cannot appear.
  HV without vmnet now behaves the same way; it has no NIC2 either
  (`darwin_hv/setup.rs` creates it only through vmnet).

## Evidence

Host: Amp Mac satellite, macos-26, Apple M4 Pro (virtual), 12 vCPU,
`kern.hv_support` absent. QEMU v0.1.0 installed from the pin, debug daemon
and CLI, release agent from this tree (`ARCBOX_AGENT`), fresh data
directories after `abctl boot prefetch`.

| Run | Bridge NIC | Container route | Host → container IP | Route warnings |
|---|---|---|---|---|
| Before (wave-3 run, master `80624dba`) | none | retried 8 s, failed | unreachable | 2 per start |
| `acceptance.sh /tmp/w4-d1 1` as the user | none: "vmnet_start_interface failed: operation failed (requires entitlement or root)"; one warning per VM start (3 in a run with a guest reboot and a restart) | not attempted | — | 0 |
| `acceptance.sh /tmp/w4-d3 3` as root, `arcbox-helper` on its own socket | `vmnet bridge NIC created mac="02:ae:1b:56:74:f2"`, the VM's stable bridge MAC, again after the guest reboot | "container route ensured (vmnet direct) bridge=bridge100" on every start and after the reboot | `curl http://172.17.0.3/` to `nginx:alpine`: "Welcome to nginx", 0.59 s | 0 |

The root run's other numbers match the wave-3 run: cold start to dockerd
12.46 s, restart to dockerd 7.38–7.69 s, warm `docker run --rm alpine echo`
1.02–1.13 s, `docker pull python:3.13-slim` 9.84 s, guest reboot to a
running container 14.29 s. After it, `ifconfig` showed no `bridge`
interface and `netstat -rn` no 172.16/12 route.

VZ and HV were not run: this Mac has no hardware virtualization. VZ's NIC2
is the same calls moved into `darwin_net`, and its `bridge_mac` is
unchanged. HV's NIC2 code is untouched; with vmnet, its `bridge_mac` is the
vmnet interface's MAC, which vmnet takes from the same stable MAC, so its
route path is unchanged.

## Follow-ups

- A development daemon that is not root gets no NIC2 on any backend that
  needs vmnet; the signed production daemon holds `com.apple.vm.networking`.
