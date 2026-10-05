//! A scoped write-and-sync probe that only removes the file it created.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Verifies file and directory durability on an already verified runtime mount.
///
/// # Errors
/// Returns any write, sync, read-back, or cleanup error with the owned path.
pub fn verify_writes(directory: &Path) -> io::Result<()> {
    let path = directory.join(format!(".arcbox-storage-check-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        let expected = uuid::Uuid::new_v4();
        file.write_all(expected.as_bytes())?;
        file.sync_all()?;
        File::open(directory)?.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let mut actual = [0_u8; 16];
        file.read_exact(&mut actual)?;
        if actual != *expected.as_bytes() {
            return Err(io::Error::other(
                "storage read-back does not match the written bytes",
            ));
        }
        Ok(())
    })();
    drop(file);
    let cleanup = fs::remove_file(&path).and_then(|()| File::open(directory)?.sync_all());
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        )),
        (Err(error), Err(cleanup)) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}; cleanup failed: {cleanup}", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn verification_removes_only_the_owned_probe_file() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join(".arcbox-storage-check-user-file");
        std::fs::write(&existing, b"preserve").unwrap();
        super::verify_writes(directory.path()).unwrap();
        assert_eq!(std::fs::read(existing).unwrap(), b"preserve");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_mount_is_not_created_by_the_probe() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-mounted");
        assert!(super::verify_writes(&missing).is_err());
        assert!(!missing.exists());
    }
}
