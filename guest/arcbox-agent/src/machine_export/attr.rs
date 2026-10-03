//! Attribute translation between the machine's filesystem and the NFS wire:
//! the ownership map, `lstat` → `fattr3`, `sattr3` → syscalls, and errno →
//! `nfsstat3`.

use std::fs::{Metadata, Permissions};
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use nfs3_server::nfs3_types::nfs3::{
    Nfs3Option, fattr3, fileid3, ftype3, nfsstat3, nfstime3, sattr3, set_atime, set_mtime,
    specdata3,
};
use nix::sys::stat::{UtimensatFlags, utimensat};
use nix::sys::time::TimeSpec;

/// The identity swap between the host user and the guest account it stands
/// in for.
///
/// The host mounts as its own uid/gid (501/20 for the first macOS user);
/// the machine's files belong to root unless the user made an account. The
/// map lets the two trade places: a root-owned file lists as the host user
/// on the Mac, a file created from the Mac lands as root in the machine,
/// and `chown` from either side follows the same rule. The swap is an
/// involution — applying it twice is the identity — so one function maps
/// both directions and no file can end up with an id neither side meant.
/// Every other id passes through unchanged, so a user the machine's owner
/// created keeps its numeric identity on the Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMap {
    host_uid: u32,
    host_gid: u32,
    guest_uid: u32,
    guest_gid: u32,
}

impl IdMap {
    pub const fn new(host_uid: u32, host_gid: u32, guest_uid: u32, guest_gid: u32) -> Self {
        Self {
            host_uid,
            host_gid,
            guest_uid,
            guest_gid,
        }
    }

    /// Maps a uid across the boundary, in either direction.
    pub const fn uid(self, uid: u32) -> u32 {
        swap(uid, self.host_uid, self.guest_uid)
    }

    /// Maps a gid across the boundary, in either direction.
    pub const fn gid(self, gid: u32) -> u32 {
        swap(gid, self.host_gid, self.guest_gid)
    }

    /// The guest owner of an object the host creates without naming one.
    pub const fn guest_owner(self) -> (u32, u32) {
        (self.guest_uid, self.guest_gid)
    }
}

const fn swap(id: u32, a: u32, b: u32) -> u32 {
    if id == a {
        b
    } else if id == b {
        a
    } else {
        id
    }
}

/// The `fattr3` of an object from its `lstat`, with ownership mapped for
/// the host.
pub fn fattr3_from_metadata(id: fileid3, meta: &Metadata, map: IdMap) -> fattr3 {
    let file_type = meta.file_type();
    let type_ = if file_type.is_file() {
        ftype3::NF3REG
    } else if file_type.is_dir() {
        ftype3::NF3DIR
    } else if file_type.is_symlink() {
        ftype3::NF3LNK
    } else if file_type.is_block_device() {
        ftype3::NF3BLK
    } else if file_type.is_char_device() {
        ftype3::NF3CHR
    } else if file_type.is_fifo() {
        ftype3::NF3FIFO
    } else {
        ftype3::NF3SOCK
    };
    fattr3 {
        type_,
        // The twelve permission bits, setuid/setgid/sticky included.
        mode: meta.mode() & 0o7777,
        nlink: u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
        uid: map.uid(meta.uid()),
        gid: map.gid(meta.gid()),
        size: meta.len(),
        used: meta.blocks().saturating_mul(512),
        rdev: rdev(meta.rdev()),
        fsid: 0,
        fileid: id,
        atime: nfstime(meta.atime(), meta.atime_nsec()),
        mtime: nfstime(meta.mtime(), meta.mtime_nsec()),
        ctime: nfstime(meta.ctime(), meta.ctime_nsec()),
    }
}

/// Device numbers for device nodes; the encoding is the guest kernel's.
#[cfg(target_os = "linux")]
fn rdev(dev: u64) -> specdata3 {
    specdata3 {
        specdata1: libc::major(dev),
        specdata2: libc::minor(dev),
    }
}

#[cfg(not(target_os = "linux"))]
fn rdev(_dev: u64) -> specdata3 {
    specdata3::default()
}

/// An `nfstime3` from `stat` fields; the protocol's seconds are unsigned
/// 32-bit, so times before 1970 clamp to the epoch.
fn nfstime(seconds: i64, nanoseconds: i64) -> nfstime3 {
    nfstime3 {
        seconds: u32::try_from(seconds.clamp(0, i64::from(u32::MAX))).unwrap_or(u32::MAX),
        nseconds: u32::try_from(nanoseconds.clamp(0, 999_999_999)).unwrap_or(0),
    }
}

/// The guest owner a newly created object gets: what the client asked for,
/// mapped, or the guest account the host stands in for.
pub fn new_object_owner(attr: &sattr3, map: IdMap) -> (u32, u32) {
    let (default_uid, default_gid) = map.guest_owner();
    let uid = match attr.uid {
        Nfs3Option::Some(uid) => map.uid(uid),
        Nfs3Option::None => default_uid,
    };
    let gid = match attr.gid {
        Nfs3Option::Some(gid) => map.gid(gid),
        Nfs3Option::None => default_gid,
    };
    (uid, gid)
}

/// Applies a `SETATTR` to the object at `path` without following a symlink.
///
/// Size first, so the truncation's own mtime update is overwritten by a
/// requested mtime rather than the other way round. Mode is skipped on a
/// symlink: Linux has no `lchmod`, and the client's chmod of a link means
/// nothing.
pub fn apply_sattr(path: &Path, attr: &sattr3, map: IdMap) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if let Nfs3Option::Some(size) = attr.size {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(size)?;
    }
    if let Nfs3Option::Some(mode) = attr.mode
        && !meta.file_type().is_symlink()
    {
        std::fs::set_permissions(path, Permissions::from_mode(mode & 0o7777))?;
    }
    let uid = match attr.uid {
        Nfs3Option::Some(uid) => Some(map.uid(uid)),
        Nfs3Option::None => None,
    };
    let gid = match attr.gid {
        Nfs3Option::Some(gid) => Some(map.gid(gid)),
        Nfs3Option::None => None,
    };
    if uid.is_some() || gid.is_some() {
        std::os::unix::fs::lchown(path, uid, gid)?;
    }
    let atime = match attr.atime {
        set_atime::DONT_CHANGE => TimeSpec::UTIME_OMIT,
        set_atime::SET_TO_SERVER_TIME => TimeSpec::UTIME_NOW,
        set_atime::SET_TO_CLIENT_TIME(t) => {
            TimeSpec::new(i64::from(t.seconds), i64::from(t.nseconds))
        }
    };
    let mtime = match attr.mtime {
        set_mtime::DONT_CHANGE => TimeSpec::UTIME_OMIT,
        set_mtime::SET_TO_SERVER_TIME => TimeSpec::UTIME_NOW,
        set_mtime::SET_TO_CLIENT_TIME(t) => {
            TimeSpec::new(i64::from(t.seconds), i64::from(t.nseconds))
        }
    };
    if atime != TimeSpec::UTIME_OMIT || mtime != TimeSpec::UTIME_OMIT {
        utimensat(None, path, &atime, &mtime, UtimensatFlags::NoFollowSymlink)?;
    }
    Ok(())
}

/// The NFS status for a filesystem error, by errno.
pub fn nfs_error(err: &io::Error) -> nfsstat3 {
    match err.raw_os_error() {
        Some(libc::EPERM) => nfsstat3::NFS3ERR_PERM,
        Some(libc::ENOENT) => nfsstat3::NFS3ERR_NOENT,
        Some(libc::ENXIO) => nfsstat3::NFS3ERR_NXIO,
        Some(libc::EACCES) => nfsstat3::NFS3ERR_ACCES,
        Some(libc::EEXIST) => nfsstat3::NFS3ERR_EXIST,
        Some(libc::EXDEV) => nfsstat3::NFS3ERR_XDEV,
        Some(libc::ENODEV) => nfsstat3::NFS3ERR_NODEV,
        Some(libc::ENOTDIR) => nfsstat3::NFS3ERR_NOTDIR,
        Some(libc::EISDIR) => nfsstat3::NFS3ERR_ISDIR,
        Some(libc::EINVAL) => nfsstat3::NFS3ERR_INVAL,
        Some(libc::EFBIG) => nfsstat3::NFS3ERR_FBIG,
        Some(libc::ENOSPC) => nfsstat3::NFS3ERR_NOSPC,
        Some(libc::EROFS) => nfsstat3::NFS3ERR_ROFS,
        Some(libc::EMLINK) => nfsstat3::NFS3ERR_MLINK,
        Some(libc::ENAMETOOLONG) => nfsstat3::NFS3ERR_NAMETOOLONG,
        Some(libc::ENOTEMPTY) => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(libc::EDQUOT) => nfsstat3::NFS3ERR_DQUOT,
        Some(libc::ESTALE) => nfsstat3::NFS3ERR_STALE,
        Some(libc::ENOTSUP) => nfsstat3::NFS3ERR_NOTSUPP,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

#[cfg(test)]
mod tests {
    use nfs3_server::nfs3_types::nfs3::{set_gid3, set_mode3, set_size3, set_uid3};

    use super::*;

    const MAP: IdMap = IdMap::new(501, 20, 0, 0);

    #[test]
    fn the_id_map_swaps_the_pair_and_passes_everything_else() {
        assert_eq!(MAP.uid(0), 501, "root-owned shows as the host user");
        assert_eq!(MAP.uid(501), 0, "and the host user lands as root");
        assert_eq!(MAP.uid(1000), 1000);
        assert_eq!(MAP.gid(0), 20);
        assert_eq!(MAP.gid(20), 0);
        for id in [0, 501, 1000, u32::MAX] {
            assert_eq!(MAP.uid(MAP.uid(id)), id, "an involution");
        }
    }

    #[test]
    fn a_new_object_defaults_to_the_guest_owner() {
        assert_eq!(new_object_owner(&sattr3::default(), MAP), (0, 0));
        let asked = sattr3 {
            uid: set_uid3::Some(501),
            gid: set_gid3::Some(1000),
            ..sattr3::default()
        };
        assert_eq!(new_object_owner(&asked, MAP), (0, 1000));
    }

    #[test]
    fn metadata_becomes_fattr3_with_the_owner_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"hello").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o640)).unwrap();
        let meta = std::fs::symlink_metadata(&file).unwrap();
        let map = IdMap::new(4242, 4343, meta.uid(), meta.gid());
        let attr = fattr3_from_metadata(7, &meta, map);
        assert_eq!(attr.type_, ftype3::NF3REG);
        assert_eq!(attr.mode, 0o640);
        assert_eq!(attr.size, 5);
        assert_eq!(attr.fileid, 7);
        assert_eq!((attr.uid, attr.gid), (4242, 4343));
        assert!(attr.mtime.seconds > 1_600_000_000);

        let link = dir.path().join("l");
        std::os::unix::fs::symlink("f", &link).unwrap();
        let link_meta = std::fs::symlink_metadata(&link).unwrap();
        assert_eq!(
            fattr3_from_metadata(8, &link_meta, map).type_,
            ftype3::NF3LNK
        );
        assert_eq!(
            fattr3_from_metadata(9, &std::fs::metadata(dir.path()).unwrap(), map).type_,
            ftype3::NF3DIR
        );
    }

    #[test]
    fn times_clamp_into_the_protocol_range() {
        assert_eq!(
            nfstime(-5, 7),
            nfstime3 {
                seconds: 0,
                nseconds: 7
            }
        );
        assert_eq!(nfstime(i64::MAX, 0).seconds, u32::MAX);
        assert_eq!(nfstime(1, 5_000_000_000).nseconds, 999_999_999);
    }

    #[test]
    fn setattr_applies_size_mode_and_client_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"0123456789").unwrap();
        let attr = sattr3 {
            mode: set_mode3::Some(0o600),
            size: set_size3::Some(4),
            mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                seconds: 1_700_000_000,
                nseconds: 0,
            }),
            ..sattr3::default()
        };
        apply_sattr(&file, &attr, MAP).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.len(), 4);
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(
            meta.mtime(),
            1_700_000_000,
            "the requested mtime survives the truncation"
        );
    }

    #[test]
    fn errno_maps_to_the_matching_nfs_status() {
        let missing = std::fs::metadata("/definitely/not/here").unwrap_err();
        assert_eq!(nfs_error(&missing), nfsstat3::NFS3ERR_NOENT);
        assert_eq!(
            nfs_error(&io::Error::from_raw_os_error(libc::ENOTEMPTY)),
            nfsstat3::NFS3ERR_NOTEMPTY
        );
        assert_eq!(
            nfs_error(&io::Error::other("no errno at all")),
            nfsstat3::NFS3ERR_IO
        );
    }
}
