//! The host side of an NFS mount: what sits at a mount point, and how to
//! mount or unmount it without waiting forever.
//!
//! Shared by the `~/ArcBox` docker-data mount (`nfs_mount`) and the
//! per-machine root mounts (`machine_mount`). Every external command here is
//! bounded: `mount_nfs` and `umount` both block inside the kernel while an
//! NFS server is unresponsive, and an unbounded wait on either keeps the
//! daemon alive after it has logged "ArcBox daemon stopped".

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// How long one `mount_nfs` invocation may take before it is killed and the
/// attempt counts as failed. A healthy mount completes in well under a
/// second; a server that stops answering mid-handshake otherwise holds the
/// process — and the thread waiting on it — until the kernel gives up.
pub const MOUNT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Shutdown may make two attempts; their 10s total is part of launchd's
/// 45s budget.
pub const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// What is mounted at a path: the `mount` command's source and fstype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub source: String,
    pub fstype: String,
}

/// Symlinks followed at most while resolving a mount point.
const MAX_SYMLINK_HOPS: usize = 16;

/// The mount whose mount point is `path`, if any.
///
/// The kernel names a mount point by its resolved path — `/private/var/
/// folders/...` for a data dir under `/var/folders` — so a literal
/// comparison never matched one, and a daemon whose mount point sat behind
/// a symlink neither recognized its own stale mount nor unmounted it at
/// shutdown (the e2e harness then hung in `TempDir::drop` on the orphaned
/// mount). But the path is never resolved by a `stat` of the mount point
/// itself: that `stat` is answered by the filesystem mounted there, and
/// hangs when its server is gone — the `~/ArcBox` export once the VM has
/// stopped. So only the parent is canonicalized, the final component is
/// looked up in the mount table as is, and `lstat`'d — local for anything
/// but a mount point — only when the table has no entry, in case it is a
/// symlink to the real mount point. A path that does not exist has nothing
/// mounted at it.
pub fn current_mount_info(path: &Path) -> Option<MountInfo> {
    let output = Command::new("/sbin/mount").output().ok()?;
    if !output.status.success() {
        return None;
    }
    resolve_mount(&String::from_utf8_lossy(&output.stdout), path)
}

/// [`current_mount_info`] against a `/sbin/mount` listing.
fn resolve_mount(mount_output: &str, path: &Path) -> Option<MountInfo> {
    let mut path = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let parent = std::fs::canonicalize(path.parent()?).ok()?;
        let candidate = parent.join(path.file_name()?);
        if let Some(info) = find_mount(mount_output, &candidate) {
            return Some(info);
        }
        // Not a mount point, so this `lstat` is answered locally.
        let meta = std::fs::symlink_metadata(&candidate).ok()?;
        if !meta.file_type().is_symlink() {
            return None;
        }
        path = parent.join(std::fs::read_link(&candidate).ok()?);
    }
    None
}

/// Finds the mount whose mount point is `target` (already canonical) in
/// `/sbin/mount` output.
fn find_mount(mount_output: &str, target: &Path) -> Option<MountInfo> {
    mount_output
        .lines()
        .find_map(|line| match parse_mount_line(line) {
            Some((mountpoint, info)) if Path::new(mountpoint) == target => Some(info),
            _ => None,
        })
}

/// Parses one `/sbin/mount` line: `SOURCE on MOUNTPOINT (fstype, opts…)`.
pub fn parse_mount_line(line: &str) -> Option<(&str, MountInfo)> {
    let (source, rest) = line.split_once(" on ")?;
    let (mountpoint, suffix) = rest.split_once(" (")?;
    let fstype = suffix
        .split([',', ')'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    Some((
        mountpoint,
        MountInfo {
            source: source.to_string(),
            fstype,
        },
    ))
}

/// Runs `mount_nfs -o <opts> <source> <mount_path>` once.
///
/// The process is killed when it outlives [`MOUNT_ATTEMPT_TIMEOUT`]; the
/// caller decides whether to retry. The error carries `mount_nfs`'s own
/// diagnostic, which is the only place the kernel's refusal is spelled out.
pub async fn mount_nfs(opts: &str, source: &str, mount_path: &Path) -> Result<()> {
    let mut command = tokio::process::Command::new("/sbin/mount_nfs");
    command
        .arg("-o")
        .arg(opts)
        .arg(source)
        .arg(mount_path)
        .kill_on_drop(true);
    let output = tokio::time::timeout(MOUNT_ATTEMPT_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("mount_nfs did not return within {MOUNT_ATTEMPT_TIMEOUT:?}"))?
        .context("failed to execute mount_nfs")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "mount_nfs exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// Unmounts `path`, waiting at most [`UNMOUNT_TIMEOUT`].
pub async fn unmount(path: &Path) -> Result<()> {
    run_umount(path, &[]).await
}

/// Forcibly unmounts `path` (`umount -f`): the NFS client abandons its
/// outstanding requests instead of waiting for a server that is gone.
pub async fn unmount_force(path: &Path) -> Result<()> {
    run_umount(path, &["-f"]).await
}

async fn run_umount(path: &Path, flags: &[&str]) -> Result<()> {
    let mut command = tokio::process::Command::new("/sbin/umount");
    command.args(flags).arg(path).kill_on_drop(true);
    let output = tokio::time::timeout(UNMOUNT_TIMEOUT, command.output())
        .await
        .context("umount timed out")??;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "umount exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MountInfo, find_mount, parse_mount_line, resolve_mount};

    /// The mount point is found through `/var` → `/private/var` and
    /// through a symlink at the final component, without the mount point
    /// itself ever being `stat`'d: nothing is mounted at it here, yet the
    /// table entry built from its parent's canonical path matches.
    #[test]
    fn a_mount_point_resolves_through_symlinks_above_and_at_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap().join("real");
        let table = format!(
            "/dev/disk3s1 on / (apfs, local, journaled)\n\
             192.168.64.3:/ on {} (nfs, nodev, nosuid, mounted by Xuan)\n",
            canonical.display()
        );
        let info = MountInfo {
            source: "192.168.64.3:/".into(),
            fstype: "nfs".into(),
        };
        assert_eq!(
            resolve_mount(&table, &dir.path().join("real")),
            Some(info.clone())
        );
        assert_eq!(resolve_mount(&table, &dir.path().join("link")), Some(info));
        assert_eq!(resolve_mount(&table, &dir.path().join("missing")), None);
        assert_eq!(resolve_mount(&table, dir.path()), None, "a plain directory");
    }

    #[test]
    fn parse_mount_line_extracts_source_and_fstype() {
        let line =
            "127.0.0.1:/run/arcbox/nfs-export/docker on /Users/t/ArcBox (nfs, nodev, read-only)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, "/Users/t/ArcBox");
        assert_eq!(
            info,
            MountInfo {
                source: "127.0.0.1:/run/arcbox/nfs-export/docker".to_string(),
                fstype: "nfs".to_string(),
            }
        );
    }

    /// The kernel reports a mount point under `/var/folders` by its resolved
    /// `/private/var/folders` path; the lookup compares against that form
    /// and matches the mount point exactly, never a parent or a child.
    #[test]
    fn find_mount_matches_the_canonical_mount_point_exactly() {
        let output = "/dev/disk3s1 on / (apfs, local, journaled)\n\
                      ArcBox:/ on /private/var/folders/x/arcbox-e2e/ArcBox (nfs, nodev, read-only)\n\
                      ArcBox:/containerd on /private/var/folders/x/arcbox-e2e/ArcBox/containerd (nfs, automounted)\n";
        let found = find_mount(
            output,
            Path::new("/private/var/folders/x/arcbox-e2e/ArcBox"),
        );
        assert_eq!(
            found,
            Some(MountInfo {
                source: "ArcBox:/".to_string(),
                fstype: "nfs".to_string(),
            })
        );
        assert_eq!(
            find_mount(output, Path::new("/private/var/folders/x/arcbox-e2e")),
            None
        );
        assert_eq!(
            find_mount(output, Path::new("/var/folders/x/arcbox-e2e/ArcBox")),
            None,
            "the caller canonicalizes; the uncanonical spelling is not in the table"
        );
    }
}
