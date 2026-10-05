use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result, VolumeRole};

const PROVISION_MAGIC: &[u8; 16] = b"ARCBOX-NEW-VOL-1";

/// Host file identity, independent of the image's filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageIdentity {
    /// Host filesystem device.
    pub device: u64,
    /// Host inode.
    pub inode: u64,
}

impl ImageIdentity {
    /// Reads the identity of an existing regular image file.
    ///
    /// # Errors
    /// Returns an error for missing files, symlinks, or non-regular files.
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(Error::RecoveryRequired(format!(
                "{} is not a regular image file",
                path.display()
            )));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// Reads an existing filesystem UUID without interpreting absence as blankness.
///
/// # Errors
/// Returns all open, seek, and read errors. `None` means only that the expected
/// primary signature is absent; the caller must not format on that evidence.
pub fn filesystem_uuid(path: &Path, role: VolumeRole) -> Result<Option<Uuid>> {
    let mut file = File::open(path)?;
    let (magic_offset, magic, uuid_offset): (u64, &[u8], u64) = match role {
        VolumeRole::Data => (0x10040, b"_BHRfS_M", 0x10020),
        VolumeRole::Metadata => (0x438, &[0x53, 0xef], 0x468),
    };
    file.seek(SeekFrom::Start(magic_offset))?;
    let mut bytes = [0_u8; 8];
    file.read_exact(&mut bytes[..magic.len()])?;
    if &bytes[..magic.len()] != magic {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(uuid_offset))?;
    let mut uuid = [0_u8; 16];
    file.read_exact(&mut uuid)?;
    Ok(Some(Uuid::from_bytes(uuid)))
}

pub fn write_provision_header(file: &mut File, uuid: Uuid) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(PROVISION_MAGIC)?;
    file.write_all(uuid.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Checks a newly created image's one-time provisioning identity.
///
/// # Errors
/// Returns an error when the header is unreadable or does not match the manifest.
pub fn verify_new_volume(path: &Path, uuid: Uuid) -> Result<()> {
    let mut bytes = [0_u8; 32];
    File::open(path)?.read_exact(&mut bytes)?;
    if &bytes[..16] != PROVISION_MAGIC || &bytes[16..] != uuid.as_bytes() {
        return Err(Error::RecoveryRequired(format!(
            "{} lacks its new-volume authorization; formatting is forbidden",
            path.display()
        )));
    }
    Ok(())
}
