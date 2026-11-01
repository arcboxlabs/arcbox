# Host sandboxes — one VM per workload on the Mac

Status: **locked** (2026-11-01). The product decision and the rules a later
change must not undo silently are recorded in
[ADR 0010](../adr/0010-sandboxes-run-on-the-host.md).

## Summary

ArcBox gains a second place to run a sandbox: directly on the Mac, one VM per
sandbox, started by the VZ or HV backend. Today every sandbox is a Firecracker
microVM nested inside the System VM, which needs nested virtualization (M3+,
VZ backend only). The host path needs neither.

This is not a new product concept. It is the same Sandbox: the same
`ComputerManager`, `SandboxService` API, `abctl sandbox` CLI and SDKs, with a
different run location. The Docker path (System VM + `dockerd`) is untouched;
the two do not depend on each other. Checkpoint (pause/resume, warm start,
templates) is required in the first version on both backends.

## Context

ArcBox runs containers as one shared Linux VM with a real `dockerd` behind the
Docker Engine API, like OrbStack. Apple's `container` gives each container its
own lightweight VM, drives only VZ through the Swift Containerization package,
and does not offer the Docker API. Three ways to get Apple's model were
compared:

| Option | What it is | Why not / why |
|---|---|---|
| A | Wrap Apple Containerization through a Swift shim | No Docker API, macOS 26 and VZ only, a second network stack; rejected |
| B | A Kata-style runtime registered in the guest `dockerd` (`docker run --runtime=…`) | Keeps Docker compatibility, but still needs nested virtualization |
| **C** | Run `ComputerManager` on the host with VZ/HV drivers | One host VM per workload, no nested virtualization; **chosen** |

Option C does not expose the Docker API. Its interface is the Sandbox API. The
only thing it shares with Docker is the OCI image format.

## Goals

- A sandbox runs as its own VM on the host, on Apple Silicon without nested
  virtualization, on both the VZ and HV backends.
- The existing sandbox contract holds on the host path: create from a
  template, executions, files, exposed ports, pause/resume with transparent
  auto-resume, idle auto-pause, templates, snapshots (`docs/sandbox-api.md`).
- E2B compatibility and the Sandbox SDKs keep working unchanged.
- The nested Firecracker path stays available.

## Non-goals

- A Docker Engine API on the host, or `docker run` creating per-container VMs.
- Adopting Apple Containerization as a backend.
- Checkpoints that move between Macs, users or macOS builds.
- Preserving network connections across a restore.

## Run locations

```diagram
┌─────────────── macOS host ─────────────────────────────────────┐
│ arcbox-daemon                                                  │
│   SandboxService ─▶ ComputerManager                            │
│                       │                                        │
│          ┌────────────┴─────────────┐                          │
│          ▼                          ▼                          │
│   host location               nested location                  │
│   VzDriver (in daemon)        System VM agent                  │
│   HvDriver ─▶ VMM process       └─▶ FcDriver ─▶ Firecracker    │
│     per sandbox                      (KVM, M3+, VZ only)       │
│          │                                                     │
│          ▼                                                     │
│   sandbox VM: vm-agent as init, rootfs ext4, own datapath      │
└────────────────────────────────────────────────────────────────┘
```

`ComputerManager` picks the location from the driver it is given. Both
locations report the same states and events. Capabilities that one location
lacks are reported through `GetCapabilities`, not hidden.

## Decided

These were settled in the design discussion or by the spikes:

1. **Option C, with sandbox merged in.** The per-workload VM is how Sandbox
   runs on the host. No new CLI, data directory or API.
2. **Both run locations coexist.** Host VMs and nested Firecracker.
3. **Both backends.** VZ and HV on the host location.
4. **Checkpoint in v1.** Not a phase-2 item.
5. **HV runs one VMM process per sandbox.** Hypervisor.framework allows one
   VM per process (`virt/arcbox-hv/src/vm.rs`), and the daemon already holds
   the System VM. This supersedes the in-process `HvDriver` in
   `docs/architecture/vm-stack-redesign.md` for sandboxes. The per-sandbox
   process also gives the driver its `Prepare`/`Staging` shape for free.
6. **VZ checkpoints need an unlocked session.** A VZ save generates a Secure
   Enclave key, which fails while the screen is locked
   ([VZ spike](../experiments/2026-10-31-vz-checkpoint-spike.md), finding 2;
   the lock as the cause is inferred, see "Checkpoint gaps"). Nothing may
   schedule a VZ save while the Mac is locked: no idle auto-pause, overnight
   agents or Runner hosts on VZ. HV checkpoints work while locked.
7. **Restore does not preserve connections.** The guest gets a vsock
   TRANSPORT_RESET and an RST for every TCP flow the checkpoint saw; segments
   of flows the new datapath never saw are reset by splicetcp. This matches
   today's contract: resume gets a fresh IP and port exposures are dropped.
8. **Rootfs is built on the host.** `oci2rootfs` and `arcbox-ext4` are pure
   Rust and already in `arcbox-computer-runtime`.
9. **The VM is the isolation boundary.** v1 does not enforce OCI namespaces,
   seccomp, capabilities or rlimits inside the VM (see "Workload contract").

## Process model

| Backend | Where the VM lives | Driver |
|---|---|---|
| VZ | In the daemon process (VZ allows many VMs per process) | `VzDriver` (`virt/arcbox-vz-driver`), gains `Checkpoint` |
| HV | One `arcbox-vmm` process per sandbox, signed with the hypervisor entitlement | New `HvDriver` that spawns and controls that process |

The HV VMM process exposes a control protocol to the daemon for start, stop,
checkpoint, restore, port forwarding, console and vsock. vsock connections
reach the daemon by fd passing over that control socket. The process is the
unit of crash isolation: a sandbox's VMM dying does not take down the daemon
or other sandboxes.

## Images and rootfs

- Templates resolve to an OCI image. `oci2rootfs` converts it into an ext4
  rootfs on the host; `arcbox-ext4` writes it.
- `vm-agent` (musl) is written into the image while it is built, through
  `oci2rootfs` 0.3.0's `Converter::convert_with` after the last layer and
  before the image is finalized. Nothing is mounted, so the build runs on the
  macOS host. `inject_vm_agent` keeps its loop mount for images that already
  exist and stays Linux-only.
- The daemon ships a musl `vm-agent` next to the guest agent.
- Per-sandbox disks are APFS `clonefile` copies of the template image instead
  of dm-snapshot: `CowManager`'s `CowBackend::Clonefile`, selected through
  `CowOptions::backend`, next to the default `CowBackend::DeviceMapper`.
- `docker:<ref>` templates are pulled from the registry. When the image is
  not in a registry and the System VM is running, it is exported from the
  System VM's `dockerd` instead (D5).

## Workload contract

The image config (Entrypoint, Cmd, Env, WorkingDir, User) and the create
request are translated into an OCI `config.json` in the consumer, and
`arcbox-oci` models it. This activates the parked `runtime/` crates under the
charter's condition ("a workspace crate must build or run an OCI runtime
bundle itself, without dockerd"). The consumer is the `workload` module of
`arcbox-computer-runtime`, and `arcbox-oci` moved next to it under
`computer/`.

- Supported: process args, env, cwd, user, tty, mounts that map to a
  VirtioFS share. The contract translates bind mounts, but `vm-agent` mounts
  no VirtioFS share yet, so no location reports `bind_mounts` in
  `GetCapabilities`.
- Rejected with an explicit error: hooks, namespaces, seccomp, capabilities,
  rlimits, devices. `Spec::validate` is shallow, so the consumer checks this
  itself.
- OCI `Status` has no Paused state. `ComputerManager` owns pause; the OCI
  state is a view of it, never the source.

## Networking

Each sandbox VM gets its own userspace datapath instance (`arcbox-net`
`NetworkDatapath`), which is what the spikes ran and what makes restored
clones independent. Port exposure goes through the driver's `PortForward`
capability instead of guest DNAT plus a host bind. The sandbox DNS name
(`<id>.sandbox.arcbox.local`) is registered through `Runtime::register_dns` at
the VM's address, as for machines. A macOS `GuestNetwork` implementation with
`reconcile` is needed; today the only one is Linux `TapNetwork`.

Host sandboxes do not reach each other directly: each VM sits behind its own
datapath, and there is no shared host bridge between them (D1).

## Checkpoint

### What the spikes proved

| Question | Result | Entry |
|---|---|---|
| Can an HV VM resume in another process? | Yes: vCPU, SIMD, sysregs, ICC, GIC blob, vtimer, RAM. MPIDR must precede the GIC blob, and ICC registers are not in it. | [spike 1](../experiments/2026-10-31-hv-checkpoint-spike.md) |
| Multi-vCPU and a virtio device under load? | Yes, 1–12 vCPUs with virtio-blk mid-I/O, once avail cursors come from `used.idx` and vCPUs are created in id order. | [spike 2](../experiments/2026-10-31-hv-checkpoint-blk-spike.md) |
| A Linux guest on the production VMM, mid-I/O? | Yes, 20/20 random pause points, 1–8 vCPUs. vCPUs stop in 1.5 ms; restore is independent of RAM size (lazy `MAP_PRIVATE`). | [spike 3](../experiments/2026-10-31-hv-checkpoint-linux-spike.md) |
| Live TCP and vsock? | Devices need only transport registers. Guests must be told: vsock TRANSPORT_RESET, injection held until handled. | [spike 4](../experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md) |
| Guests blocked in `recv`? | Saving the flow table and sending RST at SND.NXT on restore ends every flow. | [spike 5](../experiments/2026-10-31-hv-checkpoint-tcp-reset-spike.md) |
| VZ? | No production device blocks save/restore. Saving fails while locked. | [spike 6](../experiments/2026-10-31-vz-checkpoint-spike.md) |

The code from the spikes is on master: `arcbox_hv::snapshot`,
`Vmm::hv_checkpoint`/`hv_restore` (spike quality, behind the `gic` feature),
`DatapathControl`, `VirtioVsock::post_transport_reset`, and the VZ shim's
`save_state`/`restore_state`. Wave 1 wired it into the drivers: `HvDriver`
(`virt/arcbox-hv-driver`, one `arcbox-hv-vmm` process per VM) and `VzDriver`
both expose the `Checkpoint` capability and `VmDriver::restore`
([HV driver log](../logs/2026-11-01-hv-driver-process-per-vm.md), the VZ
spike's follow-up), and the VMM closed most of the HV gaps below
([HV checkpoint gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)).

### Constraints

- A checkpoint restores only on the same Mac, user and macOS build. The GIC
  blob is versioned by the framework, and the VZ state file is bound to a
  Secure Enclave key. The checkpoint records the OS build; a mismatch is
  reported, not attempted.
- Guest wall time freezes across a checkpoint. The agent's clock push runs
  after every restore, not only after boot.
- A restored NIC must keep the MAC the guest read at boot; the checkpoint
  saves it.

### Checkpoint gaps

Wave 1 closed the rows marked closed; the HV ones are measured in the
[HV checkpoint gaps experiment](../experiments/2026-11-01-hv-checkpoint-gaps.md).

| Gap | Backend | Status | Notes |
|---|---|---|---|
| VirtioFS (FUSE inode table, DAX mappings) | HV | Open | Not in any spike. Out of v1 (D3). |
| Balloon, console RX cursor (`rx_last_avail`) | HV | Closed | The balloon's target and inflation travel with the device; balloon and console resume at the guest's `used.idx` ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). |
| NIC MAC | HV | Closed | virtio-net saves and restores its MAC. The vmnet bridge NIC's MAC is saved and reused, but nothing exercises that path yet ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). |
| macOS build | HV | Closed | `state.json` records `os_build`; a restore on another build fails with `VmmError::CheckpointBuildMismatch` before building anything ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). The runtime also labels every host checkpoint with the build and refuses a mismatch first (D6). |
| DHCP lease after restore | HV | Closed | The datapath ACKs a renewal of a lease it never issued, now by unicast to the client's address, which is where a renewing client listens ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). |
| DNS registration of a restored sandbox | HV | Open | Composition (wave 2). |
| Frames queued for the RX inject thread at pause | HV | Open | Lost; TCP retransmits. Not counted. |
| Flows mid-handshake at checkpoint (`PassiveOpen`, `ActiveOpen`) | HV | Closed | A pending dial is reset like an established flow; an `ActiveOpen` saves no reset and the guest's SYN-ACK draws one ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). |
| Saving only dirty pages | HV | Closed | `mincore` picks the pages to write: a 1 GiB guest with about 50–68 MiB touched writes its RAM in about 50–59 ms at p50, instead of scanning all RAM (~300 ms per GiB) ([gaps](../experiments/2026-11-01-hv-checkpoint-gaps.md)). |
| First-touch cost of the working set after lazy restore | HV | Open | Not measured. |
| Saves with the session unlocked; restore; restore while locked; size and time for 1 GiB | VZ | Partly closed | Unlocked saves and restores work, one state file restores into several VMs at once, and size and time are measured (the [VZ spike's follow-up](../experiments/2026-10-31-vz-checkpoint-spike.md#follow-up-2026-11-01-the-unlocked-runs-restore-and-the-driver)). Open: a save and a restore while locked through the driver, and a restore on another Mac or user account. |
| `Checkpoint` capability, `RestoreSpec`, `driver_contract!` coverage | Both | Closed | `HvDriver` and `VzDriver` expose `Checkpoint` and `VmDriver::restore`, and `driver_contract!` runs its checkpoint checks against real HV processes and VZ ([HV driver log](../logs/2026-11-01-hv-driver-process-per-vm.md), the VZ follow-up). |
| `SnapshotCatalog` wiring | Both | Open | The daemon's host composition (wave 2). |
| Clone identity: MAC/IP via `NetReconfigCommand`, vsock CID, entropy reseed | Both | Open | Needed for warm start and templates. On VZ every clone keeps the source's machine identifier and MAC, so identity has to change inside the guest. |

## Capabilities and API

The Sandbox API does not change shape. `GetCapabilities` reports, per run
location and backend: `nested_virt` (nested location only), `pause_resume`,
and whether pause is available right now (false for VZ while the session is
locked). A `Pause` that cannot run returns `FAILED_PRECONDITION` with a
detail naming the reason. Bind mounts and port publishing become optional
capabilities that a location may lack.

Wave 1 put this into the proto: `CreateSandboxRequest.run_location` and one
`LocationCapability` per location in `GetCapabilitiesResponse.locations`
(backend, availability, `pause_resume`, `pause_available` and its reason,
warm prespawn, port publishing, bind mounts). `nested_virt` stays as the
nested entry's availability for older SDKs. The contract is in
`docs/sandbox-api.md`.

## Documents this changes

- [ADR 0010](../adr/0010-sandboxes-run-on-the-host.md) records the product
  decision. It supersedes `vm-stack-redesign.md`'s "sandboxes stay inside the
  System VM" row and its in-process `HvDriver` for sandboxes.
- `docs/architecture/charter.md` "`runtime/` … is parked" and
  `runtime/AGENTS.md`, in the change that adds the `arcbox-oci` consumer.
- `docs/sandbox-api.md`: templates, capabilities and pause preconditions per
  location.

## Decisions D1–D6

Locked 2026-11-01.

| # | Question | Decision | Why |
|---|---|---|---|
| D1 | Can host sandboxes reach each other? | No. Each VM is isolated behind its own datapath. | Matches E2B's model; every spike ran a datapath per VM. |
| D2 | Default run location when both work | The host. The nested location is used only when a request asks for it. | The host works on every Apple Silicon Mac; nested stays for Linux-specific needs. |
| D3 | VirtioFS bind mounts and checkpoint | A sandbox with bind mounts cannot pause in v1. `Pause` returns `FAILED_PRECONDITION` naming the reason. | FUSE and DAX state is the largest unexplored gap. |
| D4 | Default host backend | HV. VZ is selectable. | HV checkpoints work while the session is locked, and the format is ours. |
| D5 | Source of `docker:<ref>` templates on the host | The registry; when the image is not there and the System VM runs, an export from its `dockerd`. | Locally built images are the common agent case. |
| D6 | A checkpoint from another macOS build | Resume fails with a reported reason; the disk is kept and a cold boot is offered. | The disk is the user's data; the memory is not recoverable. |

## Phases

Estimates are rough engineer-weeks for one person. The pre-spike HV estimate
was 14–16 weeks; the spikes covered most of quiesce, vCPU/GIC/timer and
memory, which is why HV checkpoint is smaller here.

1. **Host location, cold boot** (≈6–8): `HvDriver` with one VMM process per
   sandbox, macOS `GuestNetwork`, `clonefile` CoW, host rootfs build with
   agent append, `arcbox-oci` consumer, `ComputerManager` running against
   host drivers. VZ through `VzDriver`.
2. **HV checkpoint productized** (≈4–5): `Checkpoint` capability over the
   spike code, balloon and console state, bridge NIC and lease handling,
   clone identity, dirty-page save, catalog wiring, e2e.
3. **VZ checkpoint** (≈2): the unlocked-session runs from the gap table,
   multi-clone restore, the lock-aware capability.
4. **Pause, warm start and templates on the host** (≈2): idle auto-pause
   honoring the VZ rule, warm pool keyed by OS build.

Phases 2 and 3 can run in parallel after phase 1. v1 ships after phase 4.

## References

- Spikes: `docs/experiments/2026-10-31-hv-checkpoint-*.md`,
  `docs/experiments/2026-10-31-vz-checkpoint-spike.md`; probes under
  `tests/bench/hv-checkpoint/` and `virt/arcbox-vz/examples/`.
- QEMU HVF vGIC series (Mohamed Mediouni, `hvf-irqchip-and-nested`) and
  `target/arm/hvf/hvf.c`: GIC and timer state on Hypervisor.framework.
- Firecracker snapshots: format, `MAP_PRIVATE`/UFFD lazy restore, clone
  uniqueness.
- Cloud Hypervisor `Snapshottable`/`Pausable`; rust-vmm `virtio-queue`
  `QueueState`.
- libkrun/krunkit: one VMM process per VM on macOS.
- Apple Containerization (`vminitd`, EXT4 formatter), Kata Containers (OCI
  fields to enforce or reject), E2B infra (templates, clones, warm pool).
