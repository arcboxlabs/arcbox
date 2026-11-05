# 2026-10-31 — Can the VZ backend checkpoint a Linux guest with Virtualization.framework's save/restore?

- Type: experiment
- Area: `virt/arcbox-vz` (shim save/restore exports, `GenericPlatform` machine identifier); input to the host-side sandbox checkpoint design
- Outcome: open (spike), stopped by a host condition. Every device set the production System VM attaches supports save/restore. Saving failed every time it was tried, because the Mac's screen was locked. Decision: **a VZ checkpoint is available only while the user's session is unlocked**. Follow-up 2026-11-01: with the session unlocked the same probe saves (16 MiB, ~130 ms for an idle 1 GiB guest) and restores, one state file restores into four VMs at once, and `VzDriver` passes the driver contract's checkpoint checks. Follow-up 2026-11-02 (locked): restore fails with "permission denied", one save 20 s after the lock succeeded, and the driver refuses both before touching the guest
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

## Follow-up (2026-11-01): the unlocked runs, restore, and the driver

Same host and build (Mac17,9, macOS 26.5 25F71), same kernel and guest,
release builds. The console session was unlocked throughout:
`IOConsoleLocked` false and no `CGSSessionScreenIsLocked` key in
`ioreg -n Root -d1 -a`. Two probes:

- the spike probe above, unchanged;
- `virt/arcbox-vz-driver/examples/checkpoint_probe.rs`, the same guest
  through `VzDriver`'s `Checkpoint` capability and `VmDriver::restore`
  (`clones <dir> <n>`: boot, checkpoint with `Resume`, keep the source
  writing its disk for 2 s, then restore `n` VMs at once, each on its own
  copy of the image's disk clone).

The "300 MiB touched" rows boot an initramfs with a 300 MiB random file
appended as a second, uncompressed cpio archive
(`head -c $((300<<20)) /dev/urandom > blob; echo blob | cpio -o -H newc`),
which the kernel unpacks into RAM.

| Run | Result |
|---|---|
| spike `save`, idle guest (3 runs) | OK; save 128–138 ms, 16 MiB state file, pause 0.2 ms, disk fsync + clone 6 ms |
| spike `save`, 300 MiB touched | OK; save 177 ms, 320 MiB state file (not sparse: 655 368 blocks) |
| spike `restore`, idle | OK; `restore_state` 178 ms, resume 0.2 ms, tick n+1, no read-back failure |
| spike `restore`, 300 MiB touched | OK; `restore_state` 412 ms |
| spike `restore` twice at once, one state file | both OK; 199 and 190 ms |
| `SPIKE_TAMPER=machine-id` / `memory` / `state` | refused: "failed to restore with error “invalid argument”" |
| `SPIKE_TAMPER=disk` (empty disk) | restored; the guest counted 15 755 read-back failures |
| driver checkpoint, idle, `Resume` | 146–147 ms (pause, save, 256 MiB disk clone, manifest) |
| driver, 4 restores at once from one image | all OK; 252–266 ms each, every clone's first tick n+1, no failures |
| driver checkpoint, 300 MiB touched / 2 restores at once | 247 ms / 544 and 599 ms, both OK |
| driver restore with another NIC MAC | VZ: "invalid argument" (the driver now refuses it first) |
| `driver_contract!` against `VzDriver` (`tests/contract.rs`) | 18/18 pass, the 4 checkpoint checks included |

In the helper's log, every save first loads a stored Secure Enclave key
(`SEPClientObject: getKey`, `SecKeyCreateWithData`) and then calls
`SecKeyCreateRandomKey` twice; that call is what failed while locked.

### Findings

4. **With the screen unlocked, the same probe, signature and host save
   and restore.** This confirms finding 2's inference as far as an
   unlocked run can: the lock state is the only thing that changed.
   Each save generates new keys, so an earlier unlocked save is not
   expected to let a later locked save through. A locked save and
   restore through the new code are below.
5. **The state file holds the guest's resident memory, and restore
   reads all of it.** 16 MiB for an idle 1 GiB guest, 320 MiB with
   300 MiB touched. The initrd buffer the kernel freed after unpacking
   (another ~300 MiB the guest had written) was not in the file, so VZ
   writes less than the guest ever touched; whether it skips freed pages
   or zeroed ones, this probe cannot tell. Save cost ~0.15 ms/MiB on top
   of a ~130 ms base. Unlike HV's lazy `MAP_PRIVATE` restore, restore time
   grows with the file: 178 ms → 412 ms.
6. **One state file restores into several VMs at once.** Every clone
   keeps the source's machine identifier and NIC MAC; VZ refuses a
   restore with another MAC. Clone identity (address, CID, entropy) on
   VZ therefore has to change inside the guest, after the restore.
7. **VZ checks the configuration but not the disk.** The disk paired
   with a state file must be the one cloned while the VM was paused; the
   driver clones writable disks into the image for that reason.
8. **The guest wall clock is frozen across a restore** (realtime +1 s
   at the first tick, whatever the gap), as on HV: the clock push after
   every restore is needed on VZ too.

### Still open

- Restore on another Mac or user account.

## Follow-up (2026-11-02): the locked runs

Same host, build, guest and probes. A watcher polled `ioreg` every 10 s,
waited until `IOConsoleLocked` had been true for 20 s, then ran each
probe once, unattended. The session locked at 01:47:42
(`IOConsoleLocked` and `CGSSessionScreenIsLocked` both true) and was
still locked when the last probe finished at 01:47:52.

| Run | Result |
|---|---|
| spike `restore` of an unlocked save | refused: `restore_state`: "failed to restore with error “permission denied”" |
| spike `save`, idle guest | OK; pause 0.2 ms, save 136 ms, 16 MiB state file, disk fsync + clone 10 ms |
| driver `checkpoint_probe locked` | `checkpoint_blocker()` is `Some(Locked)`; the checkpoint returns `Unavailable` after 0.1 ms and the guest keeps running (ticks 1 → 4, no failures) |
| driver `checkpoint_probe restore`, 2 clones of an unlocked image | both refused with `Unavailable` before VZ is called |

### Findings

9. **Restore needs an unlocked session.** VZ refuses a valid state file
   while locked with "permission denied", the same file the unlocked runs
   restored. Finding 3's first consequence holds.
10. **A save right after the lock can succeed.** This save ran 20–30 s
    after the lock and wrote a normal state file, unlike every save in the
    first runs, where the session had been locked far longer. A grace
    period after locking would explain both, but one run cannot show it,
    and nothing says how long it lasts. The decision stands unchanged:
    the driver refuses a save while locked rather than rely on a window
    it cannot see.
11. **The driver's gate works as built.** A locked save fails before the
    guest is paused and leaves it running; a locked restore fails before
    VZ is asked.
