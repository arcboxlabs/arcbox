# ADR 0005: Storage recovery repairs only listed findings, and only on a copy

- Status: accepted (2026-10-20)
- Deciders: ArcBox product owner
- Commits / PRs: 808ba32a, 3182b986, 43f70655
- Evidence: [2026-10-20 storage recovery FST repair log](../logs/2026-10-20-storage-recovery-fst-repair.md)

## Context

Storage recovery used to check the preserved pair read-only and refuse to repair anything: `docs/data-directories.md` said recovery never runs `btrfs check --repair`. On 2026-10-20 a production data disk failed that check with one finding: two free space tree entries for block groups that no longer exist (`Space key logical 1048576 length 4194304 has no corresponding block group`, and one at 5242880). Every other stage passed. The user's only remaining options were a manual repair outside ArcBox or wiping 117 GB of Docker data.

btrfs-progs `check --repair` handles this finding by clearing the free space tree (v2 cache) and rebuilding it from the extent tree (`common/clear-cache.c`, v7.1). The tree is a cache: rebuilding it does not change file data or metadata. A manual repair of a clone of that disk produced a clean read-only check, and `abctl disk recover` then passed the restart, write verification, and Docker lifecycle.

## Decision

1. Recovery repairs a finding only if this ADR lists it. The list has one entry: Btrfs free space tree entries without a block group, with no other error line in the read-only check and a metadata volume that passed.
2. The recovery guest classifies the check. A report qualifies only when every non-empty line is a known progress or summary line or one of those entries. Any line the classifier does not know disqualifies the report.
3. The guest re-classifies the attached device before it repairs, and passes only when a second read-only check is clean. The host's classification is not trusted.
4. Repair runs on a clone of the preserved data copy, never on the live image or on the preserved copy. The live image is replaced by `rename` only after the repair passed, and the preserved copy keeps the original.
5. Check-only recovery reports a repairable finding and changes nothing.

## Consequences

`docs/data-directories.md` no longer promises that recovery never runs `btrfs check --repair`; it promises item 4 instead. Adding a finding means extending the list here, the classifier, and a fixture that reproduces it. A finding whose repair changes file data or metadata trees does not belong on this list.

The classifier depends on btrfs-progs' wording, which changed in 2025 (`e2cf6a0379`: "We have a space info key for a block group that doesn't exist" became the per-entry line). A rootfs that ships an older or newer btrfs-progs must be checked against the fixture before release; a wording change makes the finding unrepairable, not misclassified.

## Alternatives considered

- Keep recovery check-only and document a manual repair: every affected user needs a Linux shell, `btrfs-progs`, and the steps on the log entry.
- Clear the tree with a `clear_cache,space_cache=v2` mount: needs a read-write mount of the copy in the recovery guest, which mounts nothing by design, and leaves no checkable before/after.
- Repair the live image in place: a failed repair would leave no original beyond the preserved copy, and recovery would have to restore it.
