//! One profile lock serializes native trust changes with daemon identity changes.

use std::{fs::File, os::unix::fs::OpenOptionsExt, path::Path};

use crate::Error;

/// Locks the profile until the returned file is dropped. Status reads do not need this lock.
pub fn lock_profile(dir: &Path) -> Result<File, Error> {
    std::fs::create_dir_all(dir).map_err(|error| Error::io(dir, error))?;
    let path = dir.join("operation.lock");
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| Error::io(&path, error))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::OperationInProgress),
        Err(std::fs::TryLockError::Error(error)) => Err(Error::io(&path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_the_same_profile_until_completion() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let operation = lock_profile(first.path()).unwrap();
        assert!(matches!(
            lock_profile(first.path()),
            Err(Error::OperationInProgress)
        ));
        let _independent = lock_profile(second.path()).unwrap();
        drop(operation);
        lock_profile(first.path()).unwrap();
    }
}
