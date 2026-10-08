//! Login-user and daemon-lock selection for privileged installation.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::{ArcboxProfile, HostLayout};

pub(super) struct Target {
    pub(super) uid: u32,
    pub(super) home: PathBuf,
    pub(super) data_dir: PathBuf,
}

impl Target {
    pub(super) fn resolve(user_uid: Option<u32>, data_dir: Option<PathBuf>) -> Result<Self> {
        let uid = match user_uid {
            Some(uid) => uid,
            None => match std::env::var("SUDO_UID") {
                Ok(value) => value.parse().context("SUDO_UID must be a user ID")?,
                Err(std::env::VarError::NotPresent) => {
                    // SAFETY: getuid has no arguments or failure case.
                    unsafe { libc::getuid() }
                }
                Err(error) => return Err(error).context("invalid SUDO_UID"),
            },
        };
        if uid == 0 {
            bail!("installation requires the login user's nonzero --user-uid or SUDO_UID");
        }
        let home = home_for_uid(uid)?;
        if !home.is_absolute() {
            bail!("user {uid} must have an absolute home directory in the account database");
        }
        let profile = ArcboxProfile::from_env_or_default();
        let data_dir = data_dir
            .or_else(|| {
                std::env::var_os(arcbox_constants::env::DATA_DIR)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| {
                home.join(match profile {
                    ArcboxProfile::Production => ".arcbox",
                    ArcboxProfile::Development => ".arcbox-dev",
                })
            });
        if !data_dir.is_absolute() {
            bail!(
                "installation data directory must be absolute: {}",
                data_dir.display()
            );
        }
        Ok(Self {
            uid,
            home,
            data_dir,
        })
    }

    pub(super) fn hold_stopped_daemon(&self) -> Result<Option<File>> {
        hold_stopped_lock(&HostLayout::new(self.data_dir.clone()).lock_file, self.uid)
    }
}

fn hold_stopped_lock(path: &Path, uid: u32) -> Result<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != uid {
        bail!(
            "{} must be a regular file owned by user {uid}",
            path.display()
        );
    }
    // SAFETY: file owns a live descriptor; the nonblocking lock does not read a PID.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            bail!(
                "stop the daemon for {} as user {uid} before installing the helper",
                path.display()
            );
        }
        return Err(error).with_context(|| format!("lock {}", path.display()));
    }
    Ok(Some(file))
}

fn home_for_uid(uid: u32) -> Result<PathBuf> {
    let mut buffer = vec![0_u8; 1024];
    loop {
        let mut entry = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable for their complete sizes; result is an output pointer.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut result,
            )
        };
        if status == libc::ERANGE {
            buffer.resize(
                buffer
                    .len()
                    .checked_mul(2)
                    .context("password database entry is too large")?,
                0,
            );
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status)).context("read login-user account");
        }
        if result.is_null() {
            bail!("no account exists for user ID {uid}");
        }
        // SAFETY: getpwuid_r initialized entry and its string pointers refer to buffer.
        let entry = unsafe { entry.assume_init() };
        // SAFETY: successful getpwuid_r returns a terminated pw_dir string in the live buffer.
        let home = unsafe { CStr::from_ptr(entry.pw_dir) };
        return Ok(PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn stopped_probe_holds_a_free_lock_and_never_reads_or_changes_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        // SAFETY: getuid has no arguments or failure case.
        let uid = unsafe { libc::getuid() };
        assert!(hold_stopped_lock(&path, uid).unwrap().is_none());
        std::fs::write(&path, "1\n").unwrap();
        let guard = hold_stopped_lock(&path, uid).unwrap().unwrap();
        assert!(
            hold_stopped_lock(&path, uid)
                .unwrap_err()
                .to_string()
                .contains("stop the daemon")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n");
        drop(guard);
        assert!(hold_stopped_lock(&path, uid).unwrap().is_some());
    }

    #[test]
    fn stopped_probe_propagates_invalid_paths_and_foreign_owners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        // SAFETY: getuid has no arguments or failure case.
        let uid = unsafe { libc::getuid() };
        assert!(hold_stopped_lock(dir.path(), uid).is_err());
        std::fs::write(&path, "1\n").unwrap();
        assert!(hold_stopped_lock(&path.join("invalid"), uid).is_err());
        assert!(hold_stopped_lock(&path, uid.wrapping_add(1)).is_err());
        std::fs::remove_file(&path).unwrap();
        symlink("missing", &path).unwrap();
        assert!(hold_stopped_lock(&path, uid).is_err());
        assert!(Target::resolve(Some(0), None).is_err());
    }
}
