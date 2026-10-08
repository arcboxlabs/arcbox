//! Serializes shared helper installation across users and profiles.

use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::privileged;

/// Holds the shared installation lock until the returned file is dropped.
pub(super) fn acquire() -> Result<File> {
    // SAFETY: geteuid has no arguments or failure case.
    if unsafe { libc::geteuid() } != 0 {
        bail!("helper installation requires administrator privileges");
    }
    acquire_at(Path::new(privileged::HELPER_INSTALL_LOCK), 0)
}

fn acquire_at(path: &Path, expected_uid: u32) -> Result<File> {
    // Keep the inode across releases so concurrent installers always lock the same file.
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open helper installation lock {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect helper installation lock {}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || has_acl(&file).context("inspect helper installation lock ACL")?
    {
        bail!(
            "{} must be a regular file owned by UID {expected_uid}, with one link, no group or other permissions, and no ACL",
            path.display()
        );
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!(
                "Another ArcBox helper installation is in progress. Retry after that installation finishes."
            );
        }
        Err(TryLockError::Error(error)) => Err(error).context("lock shared helper installation"),
    }
}

fn has_acl(file: &File) -> io::Result<bool> {
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            entry: libc::c_int,
            result: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    // SAFETY: the descriptor is live; 0x100 is macOS ACL_TYPE_EXTENDED.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // macOS reports a missing ACL as ENOENT for a live descriptor.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: acl is valid, 0 is ACL_FIRST_ENTRY, and entry is a writable pointer.
    let result = unsafe { acl_get_entry(acl, 0, &raw mut entry) };
    let error = io::Error::last_os_error();
    // SAFETY: acl was allocated by acl_get_fd_np and is no longer used.
    unsafe { acl_free(acl) };
    match result {
        0 => Ok(true),
        // macOS returns EINVAL when a valid ACL has no first entry.
        -1 if error.raw_os_error() == Some(libc::EINVAL) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn owner() -> u32 {
        // SAFETY: geteuid has no arguments or failure case.
        unsafe { libc::geteuid() }
    }

    #[test]
    fn shared_lock_excludes_other_installers_and_reuses_the_persistent_inode() {
        super::super::tests::isolated_lock_test(
            "commands::install::lock::tests::shared_lock_lifetime_worker",
        );
    }

    #[test]
    #[ignore = "Executed by the parent test in an isolated process."]
    fn shared_lock_lifetime_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("install.lock");
        let first = acquire_at(&path, owner()).unwrap();
        let inode = first.metadata().unwrap().ino();
        assert_eq!(first.metadata().unwrap().mode() & 0o777, 0o600);
        fs::write(&path, "preserve existing contents\n").unwrap();
        assert!(
            acquire_at(&path, owner())
                .unwrap_err()
                .to_string()
                .contains("Another ArcBox helper installation is in progress")
        );
        drop(first);
        let next = acquire_at(&path, owner()).unwrap();
        assert_eq!(next.metadata().unwrap().ino(), inode);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "preserve existing contents\n"
        );
        drop(next);
        assert_eq!(fs::metadata(path).unwrap().ino(), inode);
    }

    #[test]
    fn shared_lock_rejects_unsafe_metadata_and_links_without_changing_contents() {
        for change in ["group", "other", "symlink", "hardlink", "owner", "acl"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("install.lock");
            drop(acquire_at(&path, owner()).unwrap());
            fs::write(&path, "preserve\n").unwrap();
            let mut expected_uid = owner();
            match change {
                "group" => fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap(),
                "other" => fs::set_permissions(&path, fs::Permissions::from_mode(0o602)).unwrap(),
                "symlink" => {
                    let target = directory.path().join("target");
                    fs::rename(&path, &target).unwrap();
                    symlink(target, &path).unwrap();
                }
                "hardlink" => fs::hard_link(&path, directory.path().join("alias")).unwrap(),
                "owner" => expected_uid = owner().wrapping_add(1),
                "acl" => assert!(
                    std::process::Command::new("/bin/chmod")
                        .args(["+a", "everyone allow read"])
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                ),
                _ => unreachable!(),
            }
            assert!(acquire_at(&path, expected_uid).is_err(), "{change}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "preserve\n", "{change}");
        }
        let directory = tempfile::tempdir().unwrap();
        assert!(acquire_at(directory.path(), owner()).is_err());
    }
}
