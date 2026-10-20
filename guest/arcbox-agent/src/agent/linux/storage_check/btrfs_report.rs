//! Classifies a failed read-only `btrfs check` of the data volume.
//!
//! Recovery repairs one finding automatically: free space tree entries for
//! block groups that no longer exist. `btrfs check --repair` rebuilds the
//! free space tree for it and touches nothing else that read-only check
//! reported, so the finding is accepted only when every line of the report
//! is either a known progress or summary line or one of those entries. An
//! unknown line means btrfs found something else, and the volume is not
//! repairable.

/// The only error line of a stale free space tree. btrfs-progs prints it
/// from `check_free_space_tree` in `common/clear-cache.c`.
const STALE_ENTRY_PREFIX: &str = "Space key logical ";
const STALE_ENTRY_SUFFIX: &str = " has no corresponding block group";

/// Lines a read-only check prints regardless of its outcome.
const NEUTRAL_PREFIXES: &[&str] = &[
    "Opening filesystem to check...",
    "Checking filesystem on ",
    "UUID: ",
    "found ",
    "total csum bytes: ",
    "total tree bytes: ",
    "total fs tree bytes: ",
    "total extent tree bytes: ",
    "btree space waste bytes: ",
    "file data blocks allocated: ",
    " referenced ",
    "[1/8] checking log",
    "[2/8] checking root items",
    "[3/8] checking extents",
    "[4/8] checking free space tree",
    "[5/8] checking fs roots",
    "[6/8] checking only csums items",
    "[7/8] checking root refs",
    "[8/8] checking quota groups",
];

/// Returns true when the report of a failed read-only check lists only stale
/// free space tree entries, at least one of them.
pub(super) fn only_stale_free_space_tree(report: &str) -> bool {
    let mut stale = false;
    for line in report.lines().filter(|line| !line.trim().is_empty()) {
        if line.starts_with(STALE_ENTRY_PREFIX) && line.ends_with(STALE_ENTRY_SUFFIX) {
            stale = true;
        } else if !NEUTRAL_PREFIXES
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            return false;
        }
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report of a production volume on 2026-10-20 (btrfs-progs 7.1).
    const STALE: &str = "Opening filesystem to check...
Checking filesystem on /dev/vdb
UUID: 5ab1f1b7-dd5c-4345-adb8-50c309224544
found 117136371712 bytes used, error(s) found
total csum bytes: 14261848
total tree bytes: 1047232512
total fs tree bytes: 882245632
total extent tree bytes: 142426112
btree space waste bytes: 197295728
file data blocks allocated: 261959974912
 referenced 117148696576
[1/8] checking log skipped (none written)
[2/8] checking root items
[3/8] checking extents
[4/8] checking free space tree
Space key logical 1048576 length 4194304 has no corresponding block group
Space key logical 5242880 length 8388608 has no corresponding block group
[5/8] checking fs roots
[6/8] checking only csums items (without verifying data)
[7/8] checking root refs
[8/8] checking quota groups skipped (not enabled on this FS)
";

    #[test]
    fn stale_free_space_tree_entries_alone_are_repairable() {
        assert!(only_stale_free_space_tree(STALE));
    }

    #[test]
    fn any_other_error_line_is_not_repairable() {
        for extra in [
            "ERROR: errors found in fs roots",
            "parent transid verify failed on 1993001041920 wanted 259000 found 259002",
            "root 257 inode 6290 errors 2000, link count wrong",
            "ref mismatch on [1122322227200 134217728] extent item 3, found 1",
        ] {
            let report = STALE.replace(
                "[5/8] checking fs roots\n",
                &format!("[5/8] checking fs roots\n{extra}\n"),
            );
            assert!(!only_stale_free_space_tree(&report), "{extra}");
        }
    }

    #[test]
    fn a_report_without_stale_entries_is_not_this_finding() {
        let clean: String = STALE
            .lines()
            .filter(|line| !line.starts_with(STALE_ENTRY_PREFIX))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!only_stale_free_space_tree(&clean));
        assert!(!only_stale_free_space_tree(""));
    }
}
