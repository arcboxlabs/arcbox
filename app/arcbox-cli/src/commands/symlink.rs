//! Shared symlink helpers for Docker CLI tool management.
//!
//! Provides the core link/unlink logic used by multiple Docker CLI discovery
//! paths (see `self_setup/cli_tools.rs` module docs for the full list).

use std::path::{Path, PathBuf};
use std::{fs, io, os::unix::fs as unix_fs};

use arcbox_constants::paths::{DOCKER_CLI_TOOLS, is_arcbox_owned};

/// Detects the `xbin/` directory inside the current app bundle.
///
/// When `abctl` runs from `Contents/MacOS/bin/abctl`, xbin is at
/// `Contents/MacOS/xbin/`. Returns `None` outside an app bundle.
pub fn detect_bundle_xbin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // exe = …/Contents/MacOS/bin/abctl
    let xbin = exe.parent()?.parent()?.join("xbin");
    xbin.is_dir().then_some(xbin)
}

/// Outcome of inspecting an existing path before creating a symlink.
pub enum LinkAction {
    /// No file exists — safe to create.
    Create,
    /// An ArcBox-owned symlink exists but points elsewhere — remove and recreate.
    Replace,
    /// Already points to the desired target — nothing to do.
    AlreadyCurrent,
    /// Owned by another tool or is a regular file — do not touch.
    Skip(String),
}

/// Decides what to do with an existing path at `link` before symlinking to `target`.
pub fn decide_link(link: &Path, target: &Path) -> LinkAction {
    match fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(link) {
            Ok(existing) if existing == target => LinkAction::AlreadyCurrent,
            Ok(existing) if is_arcbox_owned(&existing) => LinkAction::Replace,
            Ok(existing) => {
                LinkAction::Skip(format!("owned by another tool ({})", existing.display()))
            }
            Err(e) => LinkAction::Skip(format!("could not read symlink: {e}")),
        },
        Ok(_) => LinkAction::Skip("exists and is not a symlink".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => LinkAction::Create,
        Err(e) => LinkAction::Skip(format!("stat failed: {e}")),
    }
}

/// Creates a symlink `link` → `target`, handling existing paths safely.
///
/// Returns `Ok(true)` if a symlink was created/replaced, `Ok(false)` if skipped
/// (already current or owned by another tool), `Err` on I/O failure.
#[allow(dead_code)] // Public API for future consumers (e.g. helper refactor).
pub fn safe_link(link: &Path, target: &Path) -> Result<bool, String> {
    match decide_link(link, target) {
        LinkAction::AlreadyCurrent => Ok(false),
        LinkAction::Skip(_) => Ok(false),
        LinkAction::Replace => {
            fs::remove_file(link)
                .map_err(|e| format!("failed to remove {}: {e}", link.display()))?;
            unix_fs::symlink(target, link).map_err(|e| format!("failed to create symlink: {e}"))?;
            Ok(true)
        }
        LinkAction::Create => {
            unix_fs::symlink(target, link).map_err(|e| format!("failed to create symlink: {e}"))?;
            Ok(true)
        }
    }
}

/// Removes `link` if it is a symlink pointing into an ArcBox app bundle.
///
/// Idempotent: no-op when absent or not ArcBox-owned.
pub fn safe_unlink(link: &Path) -> bool {
    if let Ok(meta) = fs::symlink_metadata(link) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(link) {
                if is_arcbox_owned(&target) {
                    let _ = fs::remove_file(link);
                    return true;
                }
            }
        }
    }
    false
}

/// Links all Docker CLI tools from `xbin_dir` into `bin_dir`.
///
/// Returns the number of tools successfully linked.
pub fn link_all_docker_tools(xbin_dir: &Path, bin_dir: &Path) -> usize {
    let mut linked = 0;
    for name in DOCKER_CLI_TOOLS {
        let target = xbin_dir.join(name);
        if !target.exists() {
            continue;
        }
        let link = bin_dir.join(name);
        match decide_link(&link, &target) {
            LinkAction::AlreadyCurrent => {
                linked += 1;
            }
            LinkAction::Skip(reason) => {
                eprintln!("Note: {}/{name}: {reason}, skipping", bin_dir.display());
            }
            action @ (LinkAction::Create | LinkAction::Replace) => {
                if matches!(action, LinkAction::Replace) {
                    if let Err(e) = fs::remove_file(&link) {
                        eprintln!("Note: could not remove {}/{name}: {e}", bin_dir.display());
                        continue;
                    }
                }
                match unix_fs::symlink(&target, &link) {
                    Ok(()) => linked += 1,
                    Err(e) => eprintln!("Note: could not create {}/{name}: {e}", bin_dir.display()),
                }
            }
        }
    }
    linked
}

/// Unlinks all ArcBox-owned Docker CLI tools from `bin_dir`.
pub fn unlink_all_docker_tools(bin_dir: &Path) {
    for name in DOCKER_CLI_TOOLS {
        safe_unlink(&bin_dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_fake_xbin(base: &Path) -> PathBuf {
        let xbin = base.join("ArcBox.app/Contents/MacOS/xbin");
        fs::create_dir_all(&xbin).unwrap();
        for name in DOCKER_CLI_TOOLS {
            fs::write(xbin.join(name), b"stub").unwrap();
        }
        xbin
    }

    #[test]
    fn link_all_creates_symlinks() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let linked = link_all_docker_tools(&xbin, &bin_dir);
        assert_eq!(linked, DOCKER_CLI_TOOLS.len());
        for name in DOCKER_CLI_TOOLS {
            assert!(
                bin_dir
                    .join(name)
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_link(bin_dir.join(name)).unwrap(), xbin.join(name));
        }
    }

    #[test]
    fn link_all_is_idempotent() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        link_all_docker_tools(&xbin, &bin_dir);
        let linked = link_all_docker_tools(&xbin, &bin_dir);
        assert_eq!(linked, DOCKER_CLI_TOOLS.len());
    }

    #[test]
    fn link_all_skips_non_arcbox_symlink() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        // Place a foreign symlink at docker
        let foreign = tmp.path().join("other/docker");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::write(&foreign, b"other").unwrap();
        unix_fs::symlink(&foreign, bin_dir.join("docker")).unwrap();

        let linked = link_all_docker_tools(&xbin, &bin_dir);
        // docker skipped, rest linked
        assert_eq!(linked, DOCKER_CLI_TOOLS.len() - 1);
        // foreign symlink untouched
        assert_eq!(fs::read_link(bin_dir.join("docker")).unwrap(), foreign);
    }

    #[test]
    fn link_all_skips_regular_file() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        fs::write(bin_dir.join("docker"), b"regular file").unwrap();

        let linked = link_all_docker_tools(&xbin, &bin_dir);
        assert_eq!(linked, DOCKER_CLI_TOOLS.len() - 1);
        assert!(
            fs::symlink_metadata(bin_dir.join("docker"))
                .unwrap()
                .file_type()
                .is_file()
        );
    }

    #[test]
    fn link_all_replaces_stale_arcbox_symlink() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let old_xbin = tmp.path().join("Old.app/Contents/MacOS/xbin");
        fs::create_dir_all(&old_xbin).unwrap();
        fs::write(old_xbin.join("docker"), b"old").unwrap();
        unix_fs::symlink(old_xbin.join("docker"), bin_dir.join("docker")).unwrap();

        let linked = link_all_docker_tools(&xbin, &bin_dir);
        assert_eq!(linked, DOCKER_CLI_TOOLS.len());
        assert_eq!(
            fs::read_link(bin_dir.join("docker")).unwrap(),
            xbin.join("docker")
        );
    }

    #[test]
    fn unlink_all_removes_arcbox_owned() {
        let tmp = tempdir().unwrap();
        let xbin = make_fake_xbin(tmp.path());
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        link_all_docker_tools(&xbin, &bin_dir);
        unlink_all_docker_tools(&bin_dir);

        for name in DOCKER_CLI_TOOLS {
            assert!(!bin_dir.join(name).exists());
        }
    }

    #[test]
    fn unlink_all_leaves_non_arcbox() {
        let tmp = tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let foreign = tmp.path().join("other/docker");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::write(&foreign, b"other").unwrap();
        unix_fs::symlink(&foreign, bin_dir.join("docker")).unwrap();

        unlink_all_docker_tools(&bin_dir);
        assert!(bin_dir.join("docker").exists());
    }

    #[test]
    fn unlink_all_is_idempotent() {
        let tmp = tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        unlink_all_docker_tools(&bin_dir); // no panic
    }
}
