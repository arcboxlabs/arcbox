# Host sandboxes v1 — execution plan

Status: **active** (2026-11-01). Executes
[the host sandboxes design](../architecture/host-sandboxes.md) under
[ADR 0011](../adr/0011-sandboxes-run-on-the-host.md). The design's decisions
are locked; this file only splits the work.

## How the work is split

Work runs in two waves. Wave 1 has seven workstreams that touch disjoint
paths and meet only at ports that already exist in
`virt/arcbox-vm-driver` (`VmDriver`, the capability traits, `GuestNetwork`,
`NetworkReconcile`) and at `CowManager`'s public API. Each workstream tests
against the `arcbox-vm-driver` `testkit` fakes (`FakeDriver`,
`FakeNetwork`, `driver_contract!`) instead of waiting for another
workstream's adapter. Wave 2 composes the pieces in the daemon.

Each workstream owns its paths below. Outside them it may only touch the
shared files listed under "Shared files", in small additive hunks.

## Wave 1

### W1 — HV driver, one VMM process per sandbox

- Owns: `virt/arcbox-vmm/src/hv_driver/**`, a new crate
  `virt/arcbox-hv-driver/**` (driver side and the VMM process binary).
- Base: the unpushed R4b commit `1f65df7b` (in-process `HvDriver` over
  `HvMachine`), rebased onto master. Its `render` (spec → `VmmConfig`)
  runs inside the sandbox's VMM process.
- Delivers: `HvDriver` that spawns one signed VMM process per sandbox and
  controls it over a Unix socket (start, stop, events, console, port
  forward, vsock by fd passing); `Prepare` (spawn without boot) and
  `Adopt` (reattach after a daemon restart); the `Checkpoint` capability
  and `VmDriver::restore` over `Vmm::hv_checkpoint`/`hv_restore`.
- Must not change: `virt/arcbox-vmm/src/vmm/darwin_hv/checkpoint/**` (W2)
  or the signatures of `Vmm::hv_checkpoint`/`hv_restore`.
- Acceptance: `driver_contract!` passes against a real HV process; two
  sandboxes run at once next to a running System VM; checkpoint → restore
  in a new process works through the driver.

### W2 — HV checkpoint gaps in the VMM

- Owns: `virt/arcbox-vmm/src/vmm/darwin_hv/**` except `hv_driver`,
  `virt/arcbox-virtio-*/**` device state, the DHCP server in
  `virt/arcbox-net/src/dhcp*`.
- Delivers, from the design's gap table: balloon and console RX state;
  the NIC MAC saved and restored; the macOS build recorded in
  `state.json` and a typed mismatch error on restore (D6); writing only
  pages the guest dirtied; a restored guest's DHCP renewal answered with
  its old address; tests for flows mid-handshake at checkpoint.
- Must keep: `Vmm::hv_checkpoint`/`hv_restore` signatures; the existing
  probes in `tests/bench/hv-checkpoint/` still pass.
- Acceptance: `run-linux.sh` and `run-net.sh` pass; save time for a 1 GiB
  guest with ~50 MiB touched is reported in the spike entries' benchmark
  follow-up.

### W3 — clonefile copy-on-write on the host

- Owns: `engine/arcbox-snapshot/src/snapshot_cow/**`.
- Delivers: a host mode of `CowManager` that clones the template image
  with APFS `clonefile` instead of dm-snapshot, selected through
  `CowOptions`. `CowManager`'s public API does not change.
- Acceptance: unit tests on macOS for create, clone, remove and crash
  recovery of half-created clones; the Linux dm path is untouched
  (`test-vm-linux` integration job).

### W4 — workload: rootfs on the host and the OCI contract

- Owns: `computer/arcbox-computer-runtime/src/rootfs/**`, a new module
  `computer/arcbox-computer-runtime/src/workload/**`, `runtime/arcbox-oci`
  (moved next to its consumer per the charter), `docs/architecture/charter.md`
  (the `runtime/` paragraph), `runtime/AGENTS.md`.
- Delivers: host rootfs build with `vm-agent` written through
  `Ext4Writer` (no loop mount); a musl `vm-agent` located the way the
  daemon locates the guest agent; registry pull of `docker:<ref>`
  templates through `oci2rootfs`'s `remote` feature; image config plus
  create request → OCI `config.json` through `arcbox-oci`, rejecting
  hooks, namespaces, seccomp, capabilities, rlimits and devices; the
  translation into `StartCommand`.
- Does not depend on unreleased `oci2rootfs` changes.
- Acceptance: a rootfs built on macOS from a registry image boots under
  W1's or the existing VZ driver in wave 2; unit tests for every rejected
  field.

### W5 — VZ checkpoint

- Owns: `virt/arcbox-vz-driver/**`, `virt/arcbox-vz/**`.
- Delivers: the `Checkpoint` capability and `VmDriver::restore` on
  `VzDriver` over the shim's `save_state`/`restore_state`, with the saved
  machine identifier and disk clone; a typed "session locked" error and a
  query the runtime can ask before scheduling a save; the open VZ runs
  from the design's gap table (unlocked save, restore, restore while
  locked, one state file into several clones, 1 GiB size and time),
  appended to the VZ spike entry as a follow-up experiment.
- Acceptance: `driver_contract!` checkpoint cases pass with the session
  unlocked. Runs that need an unlocked screen are skipped, not failed,
  while it is locked, and reported.

### W6 — macOS guest network

- Owns: a new crate `virt/arcbox-host-net/**`.
- Delivers: `GuestNetwork` + `NetworkReconcile` on macOS, one
  `NetworkDatapath` per VM, the `NicSpec` each of VZ and HV needs, no
  traffic between sandboxes (D1), the sandbox DNS name registered through
  a callback the composer supplies.
- Consumes `virt/arcbox-net` without changing it.
- Acceptance: the `GuestNetwork` contract tests from `testkit`; two VMs'
  datapaths cannot reach each other.

### W7 — ComputerManager on host drivers

- Owns: `computer/arcbox-computer-runtime/src/sandbox.rs`,
  `src/environment.rs`, the manager's capability handling, the sandbox
  proto in `rpc/` and its handlers in `app/arcbox-api`.
- Delivers: run location chosen per create (host by default, nested on
  request, D2); `Prepare`/`Staging` optional, with warm prespawn only when
  `Prepare` exists and direct paths when `Staging` is absent;
  `GetCapabilities` per location and backend, including "pause available
  now"; `Pause` refused with `FAILED_PRECONDITION` for bind mounts (D3)
  and VZ while locked; resume after a build mismatch keeps the disk and
  reports the reason (D6).
- Acceptance: runtime unit tests over `FakeDriver`/`FakeNetwork` with the
  host capability sets; the nested Firecracker path's tests unchanged.

## Wave 2

Starts when W1, W4, W6 and W7 have landed.

### W8 — daemon composition and end to end

- Wires the host location in `app/` with `HvDriver`, `VzDriver`,
  `arcbox-host-net` and the host `CowManager`; clock push after every
  restore; idle auto-pause honoring the VZ rule; warm pool keyed by the
  macOS build; `docker:<ref>` export from the System VM's `dockerd` when
  the registry has no such image (D5); `docs/sandbox-api.md`.
- Acceptance: an e2e target that creates, execs, pauses, resumes and
  removes a host sandbox on HV, and on VZ when unlocked; the Sandbox SDK
  tests pass against the host location.

## Shared files

`Cargo.toml` (workspace members, workspace dependencies), `Cargo.lock`,
`docs/README.md`, the root `AGENTS.md` component list. Add only; never
reorder or reformat them. The coordinator resolves conflicts in these
files when it integrates the branches.

## Process

- Each workstream works in its own worktree and branch
  (`feat/hs-w<N>-<slug>`), commits there under the root `AGENTS.md`
  rules, and runs the gates for the crates it touched.
- The coordinating thread rebases each finished branch onto master,
  resolves shared-file conflicts, runs the full gates, and lands it.
- A workstream that needs a change in another workstream's paths says so
  in its report instead of making it.
