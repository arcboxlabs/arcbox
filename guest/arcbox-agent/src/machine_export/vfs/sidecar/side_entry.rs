//! The side entry: where a value the filesystem will not hold on the
//! inode lives, next to its target.
//!
//! `.arcbox-xattrs/<name>` in the target's own directory is a hidden file
//! of this module's format that records the file handle it belongs to. Next to
//! the target rather than in one store under the root because the entry
//! then follows its directory through every rename for free, `rm -rf` of
//! a tree inside the machine takes the entries with it, and a stale entry
//! is recognisable from its own directory alone; the cost is a hidden
//! directory wherever the Mac has put a value that large, which is a
//! resource fork in practice. An entry whose inode is gone — the file was
//! renamed or recreated inside the machine — is dropped when next seen,
//! and a malformed entry is dropped too: a torn write left it,
//! and the Mac writes the values again when it next sets them. Unsupported
//! versions return an error and preserve the entry. The export
//! root has no directory above it, so nothing can hold its overflow.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

mod identity;
pub use identity::Identity;

/// The hidden directory holding side entries, reserved in every directory.
pub const SIDE_STORE_DIR: &str = ".arcbox-xattrs";

const SIDE_MAGIC: &[u8; 4] = b"ABXA";
const SIDE_VERSION: u16 = 2;

/// The attributes a side entry holds: Mac names and values.
pub type SideAttrs = Vec<(Vec<u8>, Vec<u8>)>;

/// The side entry follows a target renamed from `from` to `to`; whatever
/// entry the name `to` had belonged to the object the rename replaced.
pub fn follow_rename(from: &Path, to: &Path) -> io::Result<()> {
    let (Some(src), Some(dst)) = (of(from), of(to)) else {
        return Ok(());
    };
    if fs::symlink_metadata(&src).is_err() {
        return remove(&dst);
    }
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::rename(&src, &dst)?;
    if let Some(dir) = src.parent() {
        // Only an empty side store goes.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// The side entry of a target the Mac removed.
pub fn drop_for(target: &Path) -> io::Result<()> {
    of(target).map_or(Ok(()), |entry| remove(&entry))
}

/// Removes directory `path` as the Mac sees it: a directory holding only a
/// side store is empty, and the entries in it belong to files already gone.
pub fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) && only_side_store(path)? => {
            fs::remove_dir_all(path.join(SIDE_STORE_DIR))?;
            fs::remove_dir(path)
        }
        result => result,
    }
}

fn only_side_store(dir: &Path) -> io::Result<bool> {
    let mut entries = fs::read_dir(dir)?;
    let Some(first) = entries.next() else {
        return Ok(false);
    };
    Ok(first?.file_name() == SIDE_STORE_DIR && entries.next().is_none())
}

/// `<dir>/.arcbox-xattrs/<name>` for a target; `None` for the export root.
pub fn of(target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?;
    Some(target.parent()?.join(SIDE_STORE_DIR).join(name))
}

/// A side entry: magic, version, the identity, then `count` attributes as
/// (name length, value length, name, value), little-endian throughout.
pub fn write(entry: &Path, identity: &Identity, attrs: &[(&[u8], &[u8])]) -> io::Result<()> {
    let mut out = Vec::new();
    out.extend_from_slice(SIDE_MAGIC);
    out.extend_from_slice(&SIDE_VERSION.to_le_bytes());
    out.extend_from_slice(&identity.kind.to_le_bytes());
    out.extend_from_slice(&(identity.bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&identity.bytes);
    out.extend_from_slice(&(attrs.len() as u32).to_le_bytes());
    for (name, value) in attrs {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(value);
    }
    if let Some(dir) = entry.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(entry, out)
}

/// The side entry at `entry`, or `None` when there is none. One that does
/// not parse is removed. Unsupported versions preserve the file and return
/// an error because their attributes cannot be matched to the target safely.
pub fn read(entry: &Path) -> io::Result<Option<(Identity, SideAttrs)>> {
    let bytes = match fs::read(entry) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match parse(&bytes) {
        Ok(Some(parsed)) => Ok(Some(parsed)),
        Ok(None) => {
            tracing::warn!(entry = %entry.display(), "machine export: removing an unreadable side entry");
            remove(entry)?;
            Ok(None)
        }
        Err(version) => {
            let error = io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "machine export: side entry {} uses version {version}; expected {SIDE_VERSION}; preserving the file",
                    entry.display()
                ),
            );
            tracing::warn!(error = %error, "machine export: unsupported side-entry version");
            Err(error)
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Option<(Identity, SideAttrs)>, u16> {
    let Some((header, body)) = bytes.split_first_chunk::<6>() else {
        return Ok(None);
    };
    if &header[..4] != SIDE_MAGIC {
        return Ok(None);
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != SIDE_VERSION {
        return Err(version);
    }
    Ok(parse_body(body))
}

fn parse_body(bytes: &[u8]) -> Option<(Identity, SideAttrs)> {
    let mut at = 0usize;
    let mut take = |n: usize| {
        let field = bytes.get(at..at.checked_add(n)?)?;
        at += n;
        Some(field)
    };
    let kind = i32::from_le_bytes(take(4)?.try_into().ok()?);
    let handle_len = usize::from(u16::from_le_bytes(take(2)?.try_into().ok()?));
    let identity = Identity {
        kind,
        bytes: take(handle_len)?.to_vec(),
    };
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let mut attrs = Vec::new();
    for _ in 0..count {
        let name_len = usize::from(u16::from_le_bytes(take(2)?.try_into().ok()?));
        let value_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let name = take(name_len)?.to_vec();
        let value = take(value_len)?.to_vec();
        attrs.push((name, value));
    }
    Some((identity, attrs))
}

/// Removes an entry, and the side store with it when that was the last.
pub fn remove(entry: &Path) -> io::Result<()> {
    match fs::remove_file(entry) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    if let Some(dir) = entry.parent() {
        // Only an empty side store goes.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::appledouble::AppleDouble;
    use super::super::xattrs::{Mode, Store};
    use super::*;

    const STORE: Store = Store::new(64);

    fn attr(name: &str, value: &[u8]) -> super::super::appledouble::Attr {
        super::super::appledouble::Attr {
            name: name.as_bytes().to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn an_entry_sits_beside_its_target_except_at_the_root() {
        assert_eq!(of(Path::new("/")), None, "the root has no side store");
        assert_eq!(
            of(Path::new("/root/x")),
            Some(PathBuf::from("/root/.arcbox-xattrs/x"))
        );
        let identity = Identity {
            kind: 1,
            bytes: vec![7, 0, 0, 0, 3, 0, 0, 0],
        };
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join(SIDE_STORE_DIR).join("x");
        write(&entry, &identity, &[(b"a", b"1"), (b"b", &[0u8; 300])]).unwrap();
        let (read_identity, attrs) = read(&entry).unwrap().unwrap();
        assert_eq!(read_identity, identity);
        assert_eq!(
            attrs,
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), vec![0u8; 300])
            ]
        );
        assert_eq!(read(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn side_entries_follow_renames_and_removals() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let x = dir.path().join("x");
        fs::write(&x, b"body").unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![attr("small", b"s")],
            ..AppleDouble::default()
        };
        STORE.store(&x, &content, Mode::Replace).unwrap();

        // Renamed across directories: the inode keeps `small`, the side
        // entry moves along.
        let y = dir.path().join("sub/y");
        fs::rename(&x, &y).unwrap();
        follow_rename(&x, &y).unwrap();
        assert!(!dir.path().join(SIDE_STORE_DIR).exists());
        assert_eq!(STORE.load(&y).unwrap().unwrap(), content);

        // Renamed over: the replaced file's entry is gone.
        let z = dir.path().join("sub/z");
        fs::write(&z, b"z").unwrap();
        STORE.store(&z, &content, Mode::Replace).unwrap();
        fs::write(&x, b"plain").unwrap();
        fs::rename(&x, &z).unwrap();
        follow_rename(&x, &z).unwrap();
        assert_eq!(STORE.load(&z).unwrap(), None);
        assert!(
            dir.path()
                .join("sub")
                .join(SIDE_STORE_DIR)
                .join("y")
                .exists()
        );

        // Removed: the entry goes, and the emptied side store with it.
        fs::remove_file(&y).unwrap();
        drop_for(&y).unwrap();
        assert!(!dir.path().join("sub").join(SIDE_STORE_DIR).exists());

        // A directory left with only its side store counts as empty.
        let sub = dir.path().join("sub");
        fs::create_dir_all(sub.join(SIDE_STORE_DIR)).unwrap();
        fs::write(sub.join(SIDE_STORE_DIR).join("gone"), b"stale").unwrap();
        fs::remove_file(&z).unwrap();
        remove_dir(&sub).unwrap();
        assert!(!sub.exists());
        fs::create_dir_all(dir.path().join(SIDE_STORE_DIR)).unwrap();
        fs::write(dir.path().join("keep"), b"").unwrap();
        assert_eq!(
            remove_dir(dir.path()).unwrap_err().raw_os_error(),
            Some(libc::ENOTEMPTY),
            "a directory with real content stays, side store and all"
        );
        assert!(dir.path().join(SIDE_STORE_DIR).exists());
    }

    #[test]
    fn an_unsupported_version_preserves_side_and_inline_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        xattr::set(&target, "user.note", b"original").unwrap();
        let entry = of(&target).unwrap();
        fs::create_dir(entry.parent().unwrap()).unwrap();
        let legacy = b"ABXA\x01\x00legacy side-entry contents";
        fs::write(&entry, legacy).unwrap();

        let error = STORE.load(&target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("uses version 1; expected 2"));
        assert_eq!(fs::read(&entry).unwrap(), legacy);

        let replacement = AppleDouble {
            attrs: vec![attr("note", b"replacement")],
            ..AppleDouble::default()
        };
        for mode in [Mode::Replace, Mode::Merge] {
            assert_eq!(
                STORE.store(&target, &replacement, mode).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
            assert_eq!(fs::read(&entry).unwrap(), legacy);
            assert_eq!(
                xattr::get(&target, "user.note").unwrap().unwrap(),
                b"original"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn content_and_mode_changes_preserve_the_side_entry() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            ..AppleDouble::default()
        };
        STORE.store(&target, &content, Mode::Replace).unwrap();
        fs::write(&target, b"changed content").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(STORE.load(&target).unwrap().unwrap(), content);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_recreated_symlink_does_not_inherit_its_side_entry() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, b"body").unwrap();
        symlink(&target, &link).unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            ..AppleDouble::default()
        };
        STORE.store(&link, &content, Mode::Replace).unwrap();
        fs::write(&target, b"changed target").unwrap();
        assert_eq!(STORE.load(&link).unwrap().unwrap(), content);
        fs::remove_file(&link).unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(STORE.load(&link).unwrap(), None);
        assert!(!of(&link).unwrap().exists());
    }
}
