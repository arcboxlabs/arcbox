//! Where a sidecar's content lives in the machine.
//!
//! The Mac's attributes land on the target itself, as `user.*` extended
//! attributes: the name as the Mac spells it under Linux's user namespace
//! (`user.com.apple.provenance`, `user.com.apple.metadata:_kMDItemUserTags`,
//! a Mac `user.note` as `user.user.note`), Finder Info as
//! `user.com.apple.FinderInfo` and the resource fork as
//! `user.com.apple.ResourceFork`. They follow the file through every rename
//! on either side, die with it, and `getfattr -d -m - <file>` in the
//! machine shows them; an attribute set in the machine shows on the Mac
//! the same way, minus the prefix.
//!
//! A value the filesystem will not hold goes to a side entry next to the
//! target instead (`side_entry`); the Mac sees one set of attributes
//! whichever place holds each.
//!
//! The filesystem's limit is btrfs's: an attribute is one leaf item, so
//! name and value together must fit in `nodesize - 156` bytes, 16 228 on
//! the 16 KiB nodes of a machine's data disk (measured 2026-10-04, see the
//! experiment entry). Growing an attribute in place fails earlier, when
//! its leaf has no room, so one that will not replace is removed and
//! inserted afresh before it is given up on. Any other filesystem's refusal
//! takes the same way out.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;

use super::appledouble::{AppleDouble, Attr, FINDER_INFO_LEN, MAX_ATTRS, MAX_NAME_LEN};
use super::side_entry::{self, Identity};

/// btrfs's limit on an attribute's name and value together: one leaf item,
/// `nodesize - header - item - dir_item` = 16384 - 101 - 25 - 30.
pub const BTRFS_MAX_XATTR: usize = 16_228;

const NAMESPACE: &[u8] = b"user.";
const FINDER_INFO: &[u8] = b"com.apple.FinderInfo";
const RESOURCE_FORK: &[u8] = b"com.apple.ResourceFork";

/// What a stored image means for the attributes the target already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The image is the whole truth: whatever it lacks goes.
    Replace,
    /// The image is what the Mac added: whatever it lacks stays.
    Merge,
}

/// The split between the inode and the side entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Store {
    /// Name and value sizes above this go to the side entry without
    /// asking the filesystem.
    inline_limit: usize,
}

impl Store {
    pub const BTRFS: Self = Self::new(BTRFS_MAX_XATTR);

    pub const fn new(inline_limit: usize) -> Self {
        Self { inline_limit }
    }

    /// The target's sidecar content, or `None` when it has none. `ENOENT`
    /// when there is no target.
    pub fn load(self, target: &Path) -> io::Result<Option<AppleDouble>> {
        let mut content = AppleDouble::default();
        for name in user_xattrs(target)? {
            let Some(mac) = mac_name(&name) else {
                continue;
            };
            // Gone between the listing and the read: nothing to show.
            if let Some(value) = xattr::get(target, &name)? {
                place(&mut content, mac, value);
            }
        }
        if let Some(entry) = side_entry::of(target) {
            match side_entry::read(&entry)? {
                Some((identity, attrs)) if identity == Identity::of(target)? => {
                    for (name, value) in attrs {
                        place(&mut content, &name, value);
                    }
                }
                Some(_) => {
                    tracing::debug!(entry = %entry.display(), "machine export: dropping a side entry whose file is gone");
                    side_entry::remove(&entry)?;
                }
                None => {}
            }
        }
        // By name, as `copyfile(3)` orders them: the image is the same
        // however the two places listed their halves.
        content.attrs.sort_by(|a, b| a.name.cmp(&b.name));
        content.attrs.truncate(MAX_ATTRS);
        Ok((!content.is_empty()).then_some(content))
    }

    /// Stores `content` on the target: every attribute where it fits, and
    /// — in [`Mode::Replace`] — nothing else left behind in either place.
    pub fn store(self, target: &Path, content: &AppleDouble, mode: Mode) -> io::Result<()> {
        let mut wanted: Vec<(OsString, &[u8])> = content
            .attrs
            .iter()
            .map(|attr| (linux_name(&attr.name), attr.value.as_slice()))
            .collect();
        if let Some(info) = &content.finder_info {
            wanted.push((linux_name(FINDER_INFO), info));
        }
        if let Some(fork) = &content.resource_fork {
            wanted.push((linux_name(RESOURCE_FORK), fork));
        }

        let existing = user_xattrs(target)?;
        let entry = side_entry::of(target);
        let stored = entry
            .as_deref()
            .map(side_entry::read)
            .transpose()?
            .flatten();
        // Side values that stay: in a merge, those the image does not name.
        let kept: Vec<(Vec<u8>, Vec<u8>)> = match (mode, stored) {
            (Mode::Merge, Some((identity, attrs))) if identity == Identity::of(target)? => attrs
                .into_iter()
                .filter(|(name, _)| !wanted.iter().any(|(w, _)| mac_name(w) == Some(name)))
                .collect(),
            _ => Vec::new(),
        };
        let mut overflow: Vec<(&[u8], &[u8])> = Vec::new();
        for (name, value) in &wanted {
            let exists = existing.contains(name);
            if name.len() + value.len() <= self.inline_limit
                && set_inline(target, name, value, exists)?
            {
                continue;
            }
            if exists {
                remove_xattr(target, name)?;
            }
            overflow.push((mac_name(name).unwrap_or_default(), value));
        }
        if mode == Mode::Replace {
            for name in &existing {
                if !wanted.iter().any(|(wanted, _)| wanted == name) {
                    remove_xattr(target, name)?;
                }
            }
        }
        overflow.extend(
            kept.iter()
                .map(|(name, value)| (name.as_slice(), value.as_slice())),
        );

        match (entry, overflow.is_empty()) {
            (entry, true) => entry.map_or(Ok(()), |entry| side_entry::remove(&entry)),
            (None, false) => Err(io::Error::from_raw_os_error(libc::EFBIG)),
            (Some(entry), false) => {
                // After the inode writes: the first write to a lower-layer
                // file copies it up and renumbers it.
                side_entry::write(&entry, &Identity::of(target)?, &overflow)
            }
        }
    }

    /// Drops everything the Mac sees on the target; `true` when there was
    /// anything to drop.
    pub fn remove(self, target: &Path) -> io::Result<bool> {
        let mut any = false;
        for name in user_xattrs(target)? {
            remove_xattr(target, &name)?;
            any = true;
        }
        if let Some(entry) = side_entry::of(target)
            && fs::symlink_metadata(&entry).is_ok()
        {
            side_entry::remove(&entry)?;
            any = true;
        }
        Ok(any)
    }
}

/// What became of a sidecar's content moved from one target to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    /// `from` had none.
    Nothing,
    /// It is on `to` now.
    Moved,
    /// `to` does not exist yet: the content, taken off `from`, is the
    /// caller's to hold until it does.
    Parked(AppleDouble),
}

impl Store {
    /// Moves the content of `from` onto `to`, as a rename of the sidecar
    /// alone asks.
    pub fn transfer(self, from: &Path, to: &Path) -> io::Result<Transfer> {
        let content = match self.load(from) {
            Ok(Some(content)) => content,
            Ok(None) => return Ok(Transfer::Nothing),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Transfer::Nothing),
            Err(e) => return Err(e),
        };
        match fs::symlink_metadata(to) {
            Ok(_) => {
                self.store(to, &content, Mode::Replace)?;
                self.remove(from)?;
                Ok(Transfer::Moved)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.remove(from)?;
                Ok(Transfer::Parked(content))
            }
            Err(e) => Err(e),
        }
    }
}

fn linux_name(mac: &[u8]) -> OsString {
    let mut name = Vec::with_capacity(NAMESPACE.len() + mac.len());
    name.extend_from_slice(NAMESPACE);
    name.extend_from_slice(mac);
    OsString::from_vec(name)
}

fn mac_name(linux: &OsStr) -> Option<&[u8]> {
    linux.as_bytes().strip_prefix(NAMESPACE)
}

/// Puts one attribute into `content` under its Mac meaning. A later value
/// for a name replaces an earlier one: the side entry wins over the inode.
fn place(content: &mut AppleDouble, mac: &[u8], value: Vec<u8>) {
    if mac == FINDER_INFO {
        if let Ok(info) = <[u8; FINDER_INFO_LEN]>::try_from(value.as_slice())
            && info.iter().any(|&b| b != 0)
        {
            content.finder_info = Some(info);
        }
    } else if mac == RESOURCE_FORK {
        if !value.is_empty() {
            content.resource_fork = Some(value);
        }
    } else if !mac.is_empty() && mac.len() <= MAX_NAME_LEN {
        match content.attrs.iter_mut().find(|attr| attr.name == mac) {
            Some(attr) => attr.value = value,
            None => content.attrs.push(Attr {
                name: mac.to_vec(),
                value,
            }),
        }
    }
}

/// The `user.*` attributes on `target`; none on a filesystem without any.
fn user_xattrs(target: &Path) -> io::Result<Vec<OsString>> {
    match xattr::list(target) {
        Ok(names) => Ok(names.filter(|name| mac_name(name).is_some()).collect()),
        Err(e) if is_unsupported(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Sets `name` on the inode; `false` when the filesystem will not hold it.
/// btrfs grows an attribute in place only while its leaf has room, while a
/// fresh insert gets a whole item, so a refused replacement is retried as
/// remove and insert.
fn set_inline(target: &Path, name: &OsStr, value: &[u8], exists: bool) -> io::Result<bool> {
    match xattr::set(target, name, value) {
        Ok(()) => Ok(true),
        Err(e) if exists && is_no_room(&e) => {
            remove_xattr(target, name)?;
            match xattr::set(target, name, value) {
                Ok(()) => Ok(true),
                Err(e) if is_no_room(&e) => Ok(false),
                Err(e) => Err(e),
            }
        }
        Err(e) if is_no_room(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// An attribute refused for what it is rather than for who asks: too big
/// for the filesystem, or an object (a symlink, a device) Linux lets no
/// `user.*` attribute onto.
fn is_no_room(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOSPC | libc::E2BIG | libc::ERANGE | libc::EFBIG | libc::EPERM)
    ) || is_unsupported(e)
}

fn is_unsupported(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOTSUP) || e.raw_os_error() == Some(libc::EOPNOTSUPP)
}

/// Removes an attribute, an already missing one included.
fn remove_xattr(target: &Path, name: &OsStr) -> io::Result<()> {
    match xattr::remove(target, name) {
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::super::side_entry::SIDE_STORE_DIR;
    use super::*;

    /// Names and values over 64 bytes together go to the side entry, so a
    /// 100-byte fork does what a 200 KiB one does in a machine.
    const STORE: Store = Store::new(64);

    fn attr(name: &str, value: &[u8]) -> Attr {
        Attr {
            name: name.as_bytes().to_vec(),
            value: value.to_vec(),
        }
    }

    fn user_names(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = user_xattrs(path)
            .unwrap()
            .into_iter()
            .map(|name| name.into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn small_values_live_on_the_inode_and_large_ones_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        let mut info = [0u8; FINDER_INFO_LEN];
        info[8] = 0x04;
        let content = AppleDouble {
            finder_info: Some(info),
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![
                attr("com.apple.provenance", &[1, 2, 3]),
                attr("user.note", b"hi"),
            ],
        };
        STORE.store(&target, &content, Mode::Replace).unwrap();

        assert_eq!(
            user_names(&target),
            [
                "user.com.apple.FinderInfo",
                "user.com.apple.provenance",
                "user.user.note"
            ]
        );
        assert_eq!(
            xattr::get(&target, "user.user.note").unwrap().unwrap(),
            b"hi"
        );
        let side = dir.path().join(SIDE_STORE_DIR).join("x");
        assert!(side.exists(), "the fork went to the side entry");
        assert_eq!(STORE.load(&target).unwrap().unwrap(), content);
        assert_eq!(
            STORE.load(&dir.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            STORE.load(dir.path()).unwrap(),
            None,
            "the directory has nothing"
        );
    }

    #[test]
    fn storing_again_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        let first = AppleDouble {
            finder_info: None,
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![attr("a", b"1"), attr("b", &[7u8; 90])],
        };
        STORE.store(&target, &first, Mode::Replace).unwrap();
        assert_eq!(user_names(&target), ["user.a"]);
        assert_eq!(STORE.load(&target).unwrap().unwrap(), first);

        // The fork is gone, `b` shrank onto the inode, `a` grew off it.
        let second = AppleDouble {
            finder_info: None,
            resource_fork: None,
            attrs: vec![attr("a", &[1u8; 90]), attr("b", b"2")],
        };
        STORE.store(&target, &second, Mode::Replace).unwrap();
        assert_eq!(user_names(&target), ["user.b"]);
        assert_eq!(STORE.load(&target).unwrap().unwrap(), second);

        STORE
            .store(&target, &AppleDouble::default(), Mode::Replace)
            .unwrap();
        assert_eq!(STORE.load(&target).unwrap(), None);
        assert!(
            !dir.path().join(SIDE_STORE_DIR).exists(),
            "an empty side store goes"
        );

        STORE.store(&target, &first, Mode::Replace).unwrap();
        assert!(STORE.remove(&target).unwrap());
        assert!(!STORE.remove(&target).unwrap());
        assert_eq!(STORE.load(&target).unwrap(), None);
        assert!(!dir.path().join(SIDE_STORE_DIR).exists());
    }

    #[test]
    fn a_side_entry_of_a_recreated_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            ..AppleDouble::default()
        };
        STORE.store(&target, &content, Mode::Replace).unwrap();
        let side = dir.path().join(SIDE_STORE_DIR).join("x");
        assert!(side.exists());

        // Recreated inside the machine: another inode under the same name.
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"other").unwrap();
        assert_eq!(STORE.load(&target).unwrap(), None);
        assert!(!side.exists(), "the stale entry went with the lookup");

        // An entry nothing can parse goes the same way.
        fs::create_dir_all(side.parent().unwrap()).unwrap();
        fs::write(&side, b"garbage").unwrap();
        assert_eq!(STORE.load(&target).unwrap(), None);
        assert!(!side.exists());
    }

    #[test]
    fn a_merge_adds_to_what_the_target_has() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        let first = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![attr("a", b"1"), attr("big", &[7u8; 90])],
            ..AppleDouble::default()
        };
        STORE.store(&target, &first, Mode::Replace).unwrap();
        // What the Mac adds without having read the rest: `a` changes,
        // `b` appears, the fork and `big` stay.
        let added = AppleDouble {
            attrs: vec![attr("a", b"2"), attr("b", b"3")],
            ..AppleDouble::default()
        };
        STORE.store(&target, &added, Mode::Merge).unwrap();
        let merged = STORE.load(&target).unwrap().unwrap();
        assert_eq!(merged.resource_fork, first.resource_fork);
        assert_eq!(
            merged.attrs,
            [attr("a", b"2"), attr("b", b"3"), attr("big", &[7u8; 90])]
        );
        // A side value the merge names moves with its new size.
        let shrunk = AppleDouble {
            attrs: vec![attr("big", b"small now")],
            ..AppleDouble::default()
        };
        STORE.store(&target, &shrunk, Mode::Merge).unwrap();
        assert_eq!(user_names(&target), ["user.a", "user.b", "user.big"]);
        let merged = STORE.load(&target).unwrap().unwrap();
        assert_eq!(
            merged.resource_fork, first.resource_fork,
            "the fork is still beside the file"
        );
        assert_eq!(merged.attrs.len(), 3);
    }

    #[test]
    fn content_moves_between_targets_or_waits_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![attr("small", b"s")],
            ..AppleDouble::default()
        };
        assert_eq!(STORE.transfer(&a, &b).unwrap(), Transfer::Nothing);
        STORE.store(&a, &content, Mode::Replace).unwrap();
        assert_eq!(STORE.transfer(&a, &b).unwrap(), Transfer::Moved);
        assert_eq!(STORE.load(&a).unwrap(), None);
        assert_eq!(STORE.load(&b).unwrap().unwrap(), content);
        assert_eq!(STORE.transfer(&b, &c).unwrap(), Transfer::Parked(content));
        assert_eq!(STORE.load(&b).unwrap(), None);
        assert!(!dir.path().join(SIDE_STORE_DIR).exists());
    }

    #[test]
    fn names_cross_the_namespace_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        fs::write(&target, b"body").unwrap();
        // Set inside the machine: shows on the Mac without the prefix.
        xattr::set(&target, "user.mime_type", b"text/plain").unwrap();
        xattr::set(
            &target,
            "user.com.apple.FinderInfo",
            &[0u8; FINDER_INFO_LEN],
        )
        .unwrap();
        let loaded = STORE.load(&target).unwrap().unwrap();
        assert_eq!(loaded.attrs, [attr("mime_type", b"text/plain")]);
        assert_eq!(loaded.finder_info, None, "all-zero Finder Info is none");
    }
}
