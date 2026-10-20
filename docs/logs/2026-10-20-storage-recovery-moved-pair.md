# 2026-10-20 — Storage recovery rebinds a moved pair and boots the current recovery init

- Type: incident
- Area: `arcbox-storage` (`provision`), `arcbox-engine` (`vm_lifecycle::storage`, `machine::maintenance`), `arcbox-core` (`runtime::storage_recovery`), `arcbox-e2e` (`storage_recovery`)
- Commits / PRs: this change
- Related: none

## Problem or trigger

After a developer machine upgraded to the 0.9.0 daemon, the System VM did not start and Docker requests failed with `System VM storage is held for recovery`. The startup check reported `docker.img was replaced; restore the paired images`. The storage manifest recorded inodes 3002681 and 86041802; the images had inodes 244339291 and 244339290. Both superblocks still carried the recorded filesystem UUIDs (btrfs `5ab1f1b7-…`, ext4 `4702ab65-…`), and the files' birth times were unchanged. The pair was therefore the same data with new host identities, most likely from a copy or restore on 2026-10-10. Daemons before 0.9.0 do not check host identities, so the drift showed only at the upgrade.

`abctl disk recover` then failed in two places:

1. The offline check VM never answered (`vsock connect failed … port 1024 timed out`). Recovery boots the persisted System VM record, which still named the boot 0.8.8 kernel and rootfs. The 0.8.8 rootfs has no `/sbin/arcbox-storage-recovery`; 0.8.9 has it. Normal boot replaces such a record through its drift check, but a held VM never boots normally, so the record never moved forward.
2. After a passing check, recovery called `verify_pair` on the originals before restart. That repeats the same identity comparison that started the hold, so a moved pair could never leave recovery. `StorageManifest::rebind_images` existed but only tests called it.

## What was done

- `VmLifecycleManager::storage_recovery_machine` builds the record from the pinned, verified boot cache. It replaces an existing record whose kernel or rootfs differs, through the new `StorageMaintenance::replace_system_machine`. If the boot cache cannot be verified, it keeps the existing record so the disks can still be preserved. Only the record changes; the disk pair is reused unchanged.
- `arcbox_storage::rebind_pair` rebinds the live pair to its current host identities after both filesystem UUIDs match, and saves the manifest only when an identity changed. Recovery calls it in place of `verify_pair` after the offline check passes. The preserve step has already copied the replaced manifest into the recovery directory.

## Evidence

- `cargo test -p arcbox-storage`: `a_moved_pair_rebinds_in_place_and_a_foreign_one_does_not` covers a copied-back pair and a foreign metadata filesystem, which is refused with the manifest unchanged.
- `cargo test -p arcbox-engine --lib vm_lifecycle::storage`: `replaces_a_record_that_names_an_older_boot_bundle` fails without the change and passes with it; the existing tests still pass.
- `storage_recovery` e2e has a new `moved_pair_rebinds` phase: copy both stopped images out and back, observe the `was replaced` hold, run recover, and require `Complete`, a released hold and a preserved manifest. On 2026-10-20 (VZ, boot 0.8.9) the whole scenario passed in 66 s with the test daemon started with `--no-mount-nfs`, and in 82 s with the docker NFS export mounted.
- The test must run from a shell whose processes can read the NFS export. Launched from a Claude Code tool shell, it failed at `recovered daemon teardown` with SIGKILL, and an earlier version of this entry recorded that as a master defect. In that shell, listing or unmounting the export returned `EPERM` while `statfs` worked, also with the tool's sandbox disabled; an APFS image mounted in the same temporary directory unmounted normally. The daemons it spawned therefore could not release `~/ArcBox/docker`, the mount outlived the VM, and the harness killed the daemon after its 15 s grace. The same binaries run from a terminal pass, and the daemon's escalation from `umount` (`Resource busy`) to `umount -f` releases the export.

## Follow-up: the rebuilt record refused the moved pair (0.10.1)

With 0.10.1 installed, recovery on the same machine failed at once, in `Stopping`, with the original `docker.img was replaced` message; no disk was preserved. The persisted record named boot 0.8.8, so `storage_recovery_machine` replaced it, and `StorageMaintenance::create_system_machine` checked the pair with `verify_pair` before writing the new record. That is the host identity comparison recovery exists to repair, so the moved pair could never reach the offline check. The e2e did not catch it: its record already named the current boot bundle, so nothing was rebuilt.

- `create_system_machine` now checks the pair with `verify_pair_filesystems`: the manifest paths and both filesystem UUIDs, not the host identities. `rebind_pair` still records the new identities only after the offline check passes, and normal starts still run the full `verify_pair`.
- `cargo test -p arcbox-engine --lib vm_lifecycle::storage`: `builds_the_record_for_a_pair_with_new_host_identities` fails with `verify_pair` and passes with the change.
- The `moved_pair_rebinds` e2e phase also removes the System VM record, so recovery rebuilds it for the moved pair.
