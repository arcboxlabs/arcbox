# ADR 0011: Sandboxes run on the host, one VM each, with checkpoints in v1

- Status: accepted (2026-11-01); supersedes the sandbox rows of `docs/architecture/vm-stack-redesign.md` (the desktop row's "sandboxes stay inside the System VM" and the in-process `HvDriver` for sandboxes)
- Deciders: ArcBox maintainers
- Commits / PRs: the HV and VZ checkpoint spikes, `7ebe1482..8239e08e`
- Evidence: [Host sandboxes design](../architecture/host-sandboxes.md); experiments [spike 1](../experiments/2026-10-31-hv-checkpoint-spike.md), [2](../experiments/2026-10-31-hv-checkpoint-blk-spike.md), [3](../experiments/2026-10-31-hv-checkpoint-linux-spike.md), [4](../experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md), [5](../experiments/2026-10-31-hv-checkpoint-tcp-reset-spike.md), [VZ](../experiments/2026-10-31-vz-checkpoint-spike.md)

## Context

Every sandbox today is a Firecracker microVM nested inside the System VM. That
needs nested virtualization, which only the VZ backend offers and only on M3
or later. Apple's `container` shows the other model: one VM per workload,
started on the host. Three ways to get it were compared: wrapping Apple
Containerization, a Kata-style runtime inside the guest `dockerd`, and running
`ComputerManager` on the host with VZ and HV drivers. Only the last needs no
nested virtualization and keeps ArcBox's own VMM, datapath and VirtioFS.

The sandbox API promises pause/resume, warm start and templates, all of which
rest on checkpoints. The VZ driver had no checkpoint and the HV backend had
none at all, so v1 on the host was gated on whether both could be built.

## What was measured

- An HV Linux guest on the production VMM resumes in a new process from
  20/20 random pause points mid-I/O, on 1–8 vCPUs. vCPUs stop in 1.5 ms;
  restore time does not depend on RAM size (spike 3).
- Live TCP and vsock connections end with a reset after restore instead of
  hanging, once the flow table is saved and a vsock TRANSPORT_RESET is posted
  (spikes 4 and 5).
- A VZ save failed while the session was locked: Secure Enclave key generation
  returned `kIOReturnNotPermitted`. No production device blocks VZ
  save/restore. HV saves and restores worked while locked (VZ spike).

## Decision

1. A sandbox can run on the macOS host as its own VM, through the same
   `ComputerManager`, `SandboxService`, `abctl sandbox` and SDKs. There is no
   separate per-container product, CLI or Docker API for it.
2. The host location and the nested Firecracker location coexist. The host
   is the default; the nested location is used only when a request asks for
   it.
3. The host location supports both VZ and HV. HV is the default backend.
4. Checkpoint (pause/resume, warm start, templates) ships in the first
   version of the host location on both backends.
5. An HV sandbox runs in its own VMM process, one per sandbox.
6. A VZ checkpoint is taken only while the user's session is unlocked.
   Nothing schedules a VZ save while the Mac is locked, and `GetCapabilities`
   reports pause as unavailable then.
7. A checkpoint restores only on the Mac, user and macOS build that wrote it.
   On a mismatch, resume fails with the reason, the disk is kept, and a cold
   boot is offered.
8. A restore does not preserve network connections: TCP flows and vsock
   connections are reset.
9. The VM is the isolation boundary. OCI hooks, namespaces, seccomp,
   capabilities, rlimits and devices are rejected, not enforced.
10. In v1, a host sandbox with VirtioFS bind mounts cannot pause.
11. Host sandboxes do not reach each other directly; each VM has its own
    datapath.

## Consequences

- `runtime/arcbox-oci` gets its first consumer, which meets the charter's
  activation condition. The activating change updates the charter paragraph
  and `runtime/AGENTS.md`.
- `vm-stack-redesign.md`'s desktop row and its in-process `HvDriver` no longer
  describe sandboxes. A host sandbox needs an `HvDriver` that controls a
  separate process.
- Sandboxes work on M1 and M2 and on the HV backend, where they could not run
  before.
- VZ sandboxes cannot auto-pause on idle while the Mac is locked, so overnight
  agents and Runner hosts that sit locked need HV.
- Undoing decision 6 requires a measured VZ save while locked. Undoing
  decision 10 requires a checkpoint of VirtioFS's FUSE inode table and DAX
  mappings with a restore test.

## Alternatives considered

- Wrap Apple Containerization: macOS 26 and VZ only, no HV, a second network
  stack.
- A Kata-style runtime in the guest `dockerd`: keeps `docker run`, but still
  needs nested virtualization.
- Cold boot only in v1, checkpoint later: rejected; pause and warm start are
  part of the sandbox contract.
- HV in the daemon process: Hypervisor.framework allows one VM per process,
  and the daemon holds the System VM.
