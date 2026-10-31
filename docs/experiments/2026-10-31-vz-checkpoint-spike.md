# 2026-10-31 — Can the VZ backend checkpoint a Linux guest with Virtualization.framework's save/restore?

- Type: experiment
- Area: `virt/arcbox-vz` (shim save/restore exports, `GenericPlatform` machine identifier); input to the host-side sandbox checkpoint design
- Outcome: open (spike), stopped by a host condition. Every device set the production System VM attaches supports save/restore. Saving failed every time it was tried, because the Mac's screen was locked. Decision: **a VZ checkpoint is available only while the user's session is unlocked**
- Probe: `virt/arcbox-vz/examples/checkpoint_vz_spike.rs`, with the guest of [the HV Linux spike](2026-10-31-hv-checkpoint-linux-spike.md)
- Follows: [the HV TCP-reset spike](2026-10-31-hv-checkpoint-tcp-reset-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5 (25F71). Release
  build. Guest: the ArcBox kernel from boot assets 0.8.12 on 4 vCPUs with
  1 GiB RAM, the spike 3 initramfs, one 256 MiB disk, virtio-console and
  virtio-entropy. Throughout the runs the console session was locked
  (`IOConsoleLocked` and `CGSSessionScreenIsLocked` both true).

## Question

Since macOS 14, `VZVirtualMachine` can save a paused machine's state to a
file (`saveMachineStateTo(url:)`) and restore a fresh machine from it
(`restoreMachineStateFrom(url:)`). Can the VZ backend use this for
checkpoints the same way the HV backend uses its own format? Which
production devices block it, and what does the save cost?

## Method

The shim gains five exports (`Exports.swift`, mirrored in `shim_ffi.rs`):

- `abx_config_validate_save_restore` wraps `validateSaveRestoreSupport()`.
- `abx_platform_generic_machine_id_data` and `abx_platform_generic_set_machine_id`
  read and set the generic platform's `VZGenericMachineIdentifier`. A
  restore requires the identifier of the machine that saved the state, and
  a new platform gets a random one.
- `abx_vm_save_state` and `abx_vm_restore_state` wrap the two calls on the
  VM's queue.

On the Rust side these are `VirtualMachineConfiguration::validate_save_restore`,
`GenericPlatform::{machine_identifier, set_machine_identifier}`, and
`VirtualMachine::{save_state, restore_state}`.

The probe has three modes:

- `validate`: builds the base configuration (disk, console, entropy), adds
  each production device in turn, then all of them together, and runs
  `validateSaveRestoreSupport` on each.
- `save`: boots the guest. Its four threads run verified O_DIRECT I/O on
  `/dev/vda`; VZ has no HVC block path. It then pauses, saves, `fsync`s and
  clones the disk, and records the machine identifier.
- `restore`: rebuilds the configuration with that identifier, restores,
  resumes, and applies the spike 3 checks.

## Results

`validate`: every variant passes.

| Configuration | `validateSaveRestoreSupport` |
|---|---|
| base (disk, console, entropy) | supported |
| + vsock | supported |
| + NAT network | supported |
| + file-handle network (the production NIC) | supported |
| + VirtioFS share | supported |
| + Rosetta share | supported |
| + memory balloon | supported |
| + second serial port | supported |
| + nested virtualization | supported |
| production System VM set (all of the above but NAT) | supported |

`save`: the guest booted (first tick 1.27 s after `start`) and paused. The
save then failed in each of 3 runs with
`The virtual machine failed to save with error "permission denied"`, and
wrote no state file.

| Run | Result |
|---|---|
| ad-hoc signature (`arcbox.dev.entitlements`), state file under the repository | permission denied |
| ad-hoc signature, state file under `/tmp` | permission denied |
| Developer ID (`422ACSY6Y5`) with hardened runtime, state file under the repository | permission denied |

At each attempt the VZ helper process
(`com.apple.Virtualization.VirtualMachine`) logged:

```text
SecKeyCreateRandomKey_ios failed: Error Domain=NSOSStatusErrorDomain Code=-25308
"Failed to generate keypair" (errSecInteractionNotAllowed) ...
"<sepk:* kid=0000000000000000>: unable to generate key" AKSError=-536870174
```

AKS error -536870174 is `0xE00002E2`, `kIOReturnNotPermitted`. The probe
ran in the user's Aqua session (`launchctl managername`), with the console
user logged in and the screen locked.

`restore` and the negative controls (another machine identifier, a
different memory size, a flipped state byte, an empty disk) were not run.
They need a state file.

## Findings

1. **No production device blocks VZ save/restore.** This includes the
   file-handle NIC, vsock, VirtioFS, Rosetta and nested virtualization.
   The device side of a VZ checkpoint needs no workaround.
2. **A VZ save needs a Secure Enclave key, and the helper could not create
   one while the screen was locked.** VZ signs and encrypts its state
   file with a key it generates in the Secure Enclave. The path and the
   code signature made no difference. Key generation failed with
   `kIOReturnNotPermitted`, which is what the Secure Enclave returns for
   a key that requires the device to be unlocked. That the lock is the
   cause is an inference: the run where the same probe saves with the
   screen unlocked is still to do.
3. **Two consequences follow if finding 2 holds**, both untested here.
   Restore likely needs the same unlocked state, because the key is needed
   to verify and decrypt the file. And a state file is likely bound to this
   Mac and user, so it cannot be restored elsewhere.

## Decision

A VZ checkpoint is available only while the user's session is unlocked.
The checkpoint design must not schedule or depend on a VZ save while the
Mac is locked. That rules out VZ for overnight agents, idle suspend after
the user leaves, and ArcBox Runners hosts that stay locked or have nobody
logged in. Sandboxes that need checkpoints in those situations run on HV,
whose format uses no Secure Enclave. Spikes 1–5 saved and restored with
the screen locked. While locked, a VZ sandbox can still be paused in
memory, and pause/resume is unaffected.

Record this decision in an ADR when the checkpoint design is written.

## Open

- Save with the screen unlocked to confirm finding 2, then lock the screen
  and save again. This also shows whether VZ generates a key per save or
  reuses one. If it reuses one, saves made after a first unlocked save
  might succeed while locked.
- Restore after an unlocked save, and the negative controls above.
- Whether restore works while locked, and on another Mac or user account.
- Save size and time for a 1 GiB guest, to compare with HV's 300 ms
  sparse RAM write.
