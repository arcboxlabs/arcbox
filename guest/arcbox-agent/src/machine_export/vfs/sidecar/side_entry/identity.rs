use std::io;
use std::path::Path;

/// A persistent filesystem identity that includes the inode generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub(super) filesystem: [i32; 2],
    pub(super) kind: i32,
    pub(super) bytes: Vec<u8>,
}

impl Identity {
    #[cfg(target_os = "linux")]
    pub fn of(target: &Path) -> io::Result<Self> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

        use nix::fcntl::{OFlag, open};
        use nix::sys::stat::Mode;

        #[repr(C)]
        struct Handle {
            header: libc::file_handle,
            bytes: [u8; libc::MAX_HANDLE_SZ as usize],
        }
        let mut handle = Handle {
            header: libc::file_handle {
                handle_bytes: libc::MAX_HANDLE_SZ as u32,
                handle_type: 0,
                f_handle: [],
            },
            bytes: [0; libc::MAX_HANDLE_SZ as usize],
        };
        // Both native queries must refer to the same object, including a symlink itself.
        // OpenOptions masks O_PATH on musl because O_PATH is part of O_ACCMODE.
        let fd = open(
            target,
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        // SAFETY: open returned a live descriptor with no other owner.
        let file = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut mount_id = 0;
        // SAFETY: Handle supplies the advertised writable trailing buffer. All pointers stay valid for the call.
        let result = unsafe {
            libc::name_to_handle_at(
                file.as_raw_fd(),
                c"".as_ptr(),
                &raw mut handle.header,
                &raw mut mount_id,
                libc::AT_HANDLE_FID | libc::AT_EMPTY_PATH,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let fsid = nix::sys::statfs::fstatfs(&file)?.filesystem_id();
        // SAFETY: Linux libc::fsid_t is a repr(C) wrapper around two i32 values, with no padding.
        let filesystem = unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(fsid) };
        Ok(Self {
            filesystem,
            kind: handle.header.handle_type,
            bytes: handle.bytes[..handle.header.handle_bytes as usize].to_vec(),
        })
    }

    // The host runs filesystem tests; the guest always uses the Linux file handle above.
    #[cfg(all(test, target_os = "macos"))]
    pub fn of(target: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        let meta = std::fs::symlink_metadata(target)?;
        let (secs, nanos) = match meta.created()?.duration_since(UNIX_EPOCH) {
            Ok(after) => (after.as_secs().cast_signed(), after.subsec_nanos()),
            Err(before) => (
                -before.duration().as_secs().cast_signed(),
                before.duration().subsec_nanos(),
            ),
        };
        let mut bytes = meta.ino().to_le_bytes().to_vec();
        bytes.extend_from_slice(&secs.to_le_bytes());
        bytes.extend_from_slice(&nanos.to_le_bytes());
        Ok(Self {
            filesystem: [0; 2],
            kind: 0,
            bytes,
        })
    }
}
