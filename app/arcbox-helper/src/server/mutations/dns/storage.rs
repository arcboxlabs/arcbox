//! Secure resolver-directory access and generated-file ownership checks.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use arcbox_helper::HelperError;

pub(super) fn open_directory(path: &Path) -> Result<Option<File>, HelperError> {
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HelperError::io("open directory", path, error)),
    };
    let metadata = directory
        .metadata()
        .map_err(|e| HelperError::io("stat", path, e))?;
    if !private_writer(&metadata) || has_acl(&directory, path)? {
        return Err(HelperError::foreign_managed(path));
    }
    Ok(Some(directory))
}

#[derive(PartialEq, Eq)]
pub(super) enum ManagedFile {
    Missing,
    Foreign,
    Owned,
}

fn private_writer(metadata: &fs::Metadata) -> bool {
    // SAFETY: geteuid takes no arguments and has no failure case.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0
}

fn has_acl(file: &File, path: &Path) -> Result<bool, HelperError> {
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
        let error = std::io::Error::last_os_error();
        // macOS reports a missing ACL as ENOENT for a live descriptor.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(HelperError::io("read ACL", path, error))
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: acl is valid, 0 is ACL_FIRST_ENTRY, and entry is a writable pointer.
    let result = unsafe { acl_get_entry(acl, 0, &raw mut entry) };
    let error = std::io::Error::last_os_error();
    // SAFETY: acl was allocated by acl_get_fd_np and is no longer used.
    unsafe { acl_free(acl) };
    match result {
        0 => Ok(true),
        // macOS returns EINVAL when a valid ACL has no first entry.
        -1 if error.raw_os_error() == Some(libc::EINVAL) => Ok(false),
        _ => Err(HelperError::io("read ACL entry", path, error)),
    }
}

pub(super) fn inspect(
    path: &Path,
    owns: impl FnOnce(&[u8]) -> bool,
) -> Result<ManagedFile, HelperError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagedFile::Missing);
        }
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(ManagedFile::Foreign),
        Err(error) => return Err(HelperError::io("open", path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|e| HelperError::io("stat", path, e))?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || !private_writer(&metadata)
        || metadata.len() > 256
        || has_acl(&file, path)?
    {
        return Ok(ManagedFile::Foreign);
    }
    let mut content = Vec::new();
    file.take(257)
        .read_to_end(&mut content)
        .map_err(|e| HelperError::io("read", path, e))?;
    Ok(if owns(&content) {
        ManagedFile::Owned
    } else {
        ManagedFile::Foreign
    })
}
