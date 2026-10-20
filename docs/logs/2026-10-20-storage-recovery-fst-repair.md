# 2026-10-20 — Storage recovery rebuilds a stale Btrfs free space tree

- Type: change
- Area: `arcbox-agent` (`storage_check`), `arcbox-core` (`runtime::storage_recovery`), `arcbox-engine` (`agent_client::storage_check`), `arcbox-protocol` (`agent.proto`)
- Commits / PRs: 808ba32a, 3182b986, 43f70655
- Related: [ADR 0005](../adr/0005-storage-recovery-repairs-listed-findings-on-a-copy.md), [2026-10-20 moved pair](2026-10-20-storage-recovery-moved-pair.md)

## Problem or trigger

With 0.10.2 installed, recovery of a developer's moved pair reached the offline check and failed it. The read-only `btrfs check` of the data copy reported:

```text
[4/8] checking free space tree
Space key logical 1048576 length 4194304 has no corresponding block group
Space key logical 5242880 length 8388608 has no corresponding block group
```

Every other stage passed, and `e2fsck -fn` passed the metadata copy. Recovery had no way forward but a wipe. The repair below was done by hand first: an APFS clone of `docker.img`, `btrfs check --repair --force` on the clone in an isolated development daemon's alpine machine, a clean read-only re-check, the clone moved over `data/docker.img`, then `abctl disk recover`, which passed the restart, write verification, and Docker lifecycle; 43 containers and 98 images were intact.

## What was done

- `StorageCheckResult.finding` reports `STALE_FREE_SPACE_TREE` when every line of a failed read-only `btrfs check` is a known progress or summary line or a stale entry (`storage_check/btrfs_report.rs`). Any other line leaves the failure unclassified.
- `REPAIR_FREE_SPACE_TREE`, accepted only in the recovery guest, re-classifies the attached data device itself, runs `btrfs check --repair --force` only for that finding, and passes only when a second read-only check is clean. The report holds the before, repair, and after output.
- Recover clones the preserved data copy into `<recovery directory>/repair/`, boots a second disposable guest with that clone writable and the metadata copy read-only, and asks for the repair. A passed repair replaces `data/docker.img` by rename; rebind, restart, and write verification follow as before. The preserved copy keeps the original. Check-only reports the finding and changes nothing.
- The agent client's storage check deadline grew from 61 to 91 minutes: the repair runs three 30-minute tools in sequence.

## Evidence

- `btrfs_report` unit tests: the production report above is repairable; the same report plus any of four other btrfs error lines is not; a report without stale entries is not.
- `guest::verdict` unit test: only a stale data finding beside a passing metadata check is repairable; a failed metadata check, an unclassified data failure, a stale finding on the metadata role, or a single result are not.
- `preserve` unit test: the repair writes a clone, the preserved copy and the live image stay unchanged until the install, and the install replaces the live image by rename.
- `storage-recovery-linux.sh` now restores btrfs-progs' `fsck-tests/058-bad-free-space-tree-entry/corrupt-free-space-tree.img.xz` (v7.1, SHA-256 `5a2c84ef…a87f`) and runs `stale_free_space_tree_is_classified_repaired_and_rechecked` with the recovery rootfs's own `/sbin/btrfs` (btrfs-progs 7.1, boot bundle 0.8.9). On 2026-10-20 it passed in a privileged alpine container on ArcBox: the fixture classified as stale (three entries), the repair printed `deleted orphan fst entries` for each, the re-check was clean, and a second repair was refused.
- The fixture is GPL-licensed btrfs-progs test data, so the script downloads it and the repository does not store it. `btrfs-image` comes from the host; the recovery rootfs does not ship it.

## Root cause (2026-10-21)

The entries are left by `mkfs.btrfs` from btrfs-progs v6.12. mkfs builds the filesystem on temporary SINGLE chunks, records free space tree entries for them (`4b6cf2a3eb`, 2021), and removes them in `cleanup_temp_chunks` through `btrfs_remove_block_group`. Before `3fc9f1ed32` ("remove block group free space", first released in v6.17) that function did not call `remove_block_group_free_space`, so the block group items went away and their free space entries stayed. The production volume mounted and served Docker for five months with them; `btrfs check` reports them from v6.17 on.

- Reproduced on 2026-10-21 in an alpine machine: an 8 TiB sparse image formatted with boot bundle 0.5.7's `/sbin/mkfs.btrfs` (v6.12) and checked with 0.8.9's `/sbin/btrfs` (v7.1) reports exactly `Space key logical 1048576 length 4194304` and `5242880 length 8388608`. The same image formatted with 0.8.9's `mkfs.btrfs` (v7.1) checks clean.
- The production disk was created on 2026-05-26, when arcbox pinned boot bundle 0.5.7.
- boot-assets built mkfs from btrfs-progs v6.12 through v0.6.7 and from v7.1 since v0.6.8 (`ce59c69`, 2026-07-19); arcbox pinned 0.6.8 on 2026-07-19 (`175ed58b`). Data disks those bundles formatted carry these two entries; disks formatted since do not. Recovery repairs them when a read-only check runs; nothing else needs to change.

## Follow-ups

- btrfs-progs older than `e2cf6a0379` (2025-07) reports this finding as `We have a space info key for a block group that doesn't exist`. The classifier accepts only the newer wording, so such a rootfs makes the finding unrepairable rather than misclassified.
