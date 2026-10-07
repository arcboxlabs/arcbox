//! Persistent identity and initialization authority for paired runtime volumes.
//!
//! Only a newly created image with its matching provisioning header can
//! consume the durable, single-use formatting authority.

mod probe;
mod provision;

#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use probe::{ImageIdentity, filesystem_uuid, verify_new_volume};
pub use provision::{prepare_pair, verify_pair};

/// Kernel command-line key naming the manifest on the host's VirtioFS share.
pub const MANIFEST_CMDLINE_KEY: &str = "arcbox.storage_manifest=";

/// A storage operation failed before the runtime could safely use its volumes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem or image requires operator-assisted recovery.
    #[error("runtime storage needs recovery: {0}")]
    RecoveryRequired(String),
    /// A file operation failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A manifest is invalid.
    #[error("invalid storage manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    /// A manifest update did not reach confirmed durable storage.
    #[error("persist storage manifest: {0}")]
    Atomic(#[from] arcbox_atomic_file::AtomicWriteError),
}

/// Result of a storage operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Filesystem role within the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeRole {
    /// Btrfs bulk data, container layers, volumes, and sandbox data.
    Data,
    /// ext4 metadata databases used with the data volume.
    Metadata,
}

/// Durable formatting authority for one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeState {
    /// The host created this image exclusively and wrote its provisioning header.
    New,
    /// Formatting started; retrying the formatter is forbidden.
    Formatting,
    /// The expected filesystem UUID was observed after formatting or adoption.
    Ready,
}

/// Migration history needed to distinguish a first upgrade from a lost member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageLayout {
    /// Both images belong to a new installation.
    Fresh,
    /// An existing Btrfs-only installation must pass guest-side migration checks.
    LegacyMigration,
    /// Original legacy metadata was verified before migration began.
    Migrating,
    /// Both legacy filesystems exist, but metadata migration is not yet verified.
    LegacyPair,
    /// Both filesystems are initialized and their metadata mappings are prepared.
    Paired,
}

/// Identity and initialization state of one image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRecord {
    /// Image basename, relative to the manifest directory.
    pub filename: String,
    /// Host filesystem identity of the image, checked before VM attachment.
    pub image: ImageIdentity,
    /// Expected filesystem UUID, also bound into a new image's provisioning header.
    pub filesystem_uuid: Uuid,
    /// Single-use formatting state.
    pub state: VolumeState,
}

/// Durable identity of the two filesystems that constitute runtime storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageManifest {
    /// Manifest schema version.
    pub version: u32,
    /// Data image record.
    pub data: VolumeRecord,
    /// Metadata image record.
    pub metadata: VolumeRecord,
    /// Initialization and migration history.
    pub layout: StorageLayout,
}

impl StorageManifest {
    /// Loads and validates a manifest without changing either image.
    ///
    /// # Errors
    /// Returns an error for unreadable or malformed manifests.
    pub fn load(path: &Path) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(&fs::read(path)?)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Replaces the manifest with a durable atomic write.
    ///
    /// # Errors
    /// Returns an error when validation or durable persistence fails.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        arcbox_atomic_file::write(path, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::RecoveryRequired(format!(
                "unsupported storage manifest version {}",
                self.version
            )));
        }
        for record in [&self.data, &self.metadata] {
            if Path::new(&record.filename)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(record.filename.as_str())
                || record.filesystem_uuid.is_nil()
            {
                return Err(Error::RecoveryRequired("invalid volume identity".into()));
            }
        }
        if self.data.filename == self.metadata.filename
            || self.data.filesystem_uuid == self.metadata.filesystem_uuid
        {
            return Err(Error::RecoveryRequired(
                "volume identities must be distinct".into(),
            ));
        }
        let valid_states = match self.layout {
            StorageLayout::Fresh => true,
            StorageLayout::LegacyMigration => {
                self.data.state == VolumeState::Ready && self.metadata.state == VolumeState::New
            }
            StorageLayout::Migrating => self.data.state == VolumeState::Ready,
            StorageLayout::LegacyPair | StorageLayout::Paired => {
                self.data.state == VolumeState::Ready && self.metadata.state == VolumeState::Ready
            }
        };
        if !valid_states {
            return Err(Error::RecoveryRequired(
                "volume states conflict with storage layout".into(),
            ));
        }
        Ok(())
    }

    /// Returns the record for `role`.
    #[must_use]
    pub const fn volume(&self, role: VolumeRole) -> &VolumeRecord {
        match role {
            VolumeRole::Data => &self.data,
            VolumeRole::Metadata => &self.metadata,
        }
    }

    /// Returns the record for `role` for a durable state transition.
    pub const fn volume_mut(&mut self, role: VolumeRole) -> &mut VolumeRecord {
        match role {
            VolumeRole::Data => &mut self.data,
            VolumeRole::Metadata => &mut self.metadata,
        }
    }

    /// Verifies both host image identities and their expected filesystem state.
    ///
    /// # Errors
    /// Returns an error for missing, replaced, unreadable, or unrecognized images.
    pub fn verify_images(&self, directory: &Path) -> Result<()> {
        for role in [VolumeRole::Data, VolumeRole::Metadata] {
            let volume = self.volume(role);
            let path = directory.join(&volume.filename);
            if ImageIdentity::read(&path)? != volume.image {
                return Err(Error::RecoveryRequired(format!(
                    "{} was replaced; restore the paired images",
                    path.display()
                )));
            }
            self.verify_volume(role, &path)?;
        }
        Ok(())
    }

    /// Rebinds a verified recovery copy to its new host file identities.
    ///
    /// The caller must stop the VM and clone both images before this operation.
    /// This operation does not persist the result or alter filesystem identities.
    ///
    /// # Errors
    /// Returns an error unless both copies retain the expected filesystem identity.
    pub fn rebind_images(&self, directory: &Path) -> Result<Self> {
        let mut rebound = self.clone();
        for role in [VolumeRole::Data, VolumeRole::Metadata] {
            let path = directory.join(&self.volume(role).filename);
            self.verify_volume(role, &path)?;
            rebound.volume_mut(role).image = ImageIdentity::read(&path)?;
        }
        Ok(rebound)
    }

    /// Verifies a block device or image against its persisted role and state.
    ///
    /// # Errors
    /// Returns an error for an unreadable or mismatched filesystem.
    pub fn verify_volume(&self, role: VolumeRole, path: &Path) -> Result<()> {
        let record = self.volume(role);
        match record.state {
            VolumeState::New => verify_new_volume(path, record.filesystem_uuid),
            VolumeState::Formatting | VolumeState::Ready => {
                if filesystem_uuid(path, role)? != Some(record.filesystem_uuid) {
                    return Err(Error::RecoveryRequired(format!(
                        "{} does not contain the expected {role:?} filesystem; formatting is forbidden",
                        path.display()
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Formats an authorized new volume exactly once, or verifies an existing one.
///
/// The caller must serialize manifest mutations across both volumes. The
/// formatter must use the supplied UUID. After an interrupted format, only an
/// already recognizable matching filesystem can advance to `Ready`.
///
/// # Errors
/// Returns an error without invoking `format` for unknown or mismatched disks.
pub fn ensure_filesystem(
    manifest_path: &Path,
    role: VolumeRole,
    device: &Path,
    format: impl FnOnce(Uuid) -> Result<()>,
) -> Result<()> {
    let mut manifest = StorageManifest::load(manifest_path)?;
    manifest.verify_volume(role, device)?;
    if manifest.volume(role).state == VolumeState::Ready {
        return Ok(());
    }
    if manifest.volume(role).state == VolumeState::New {
        if role == VolumeRole::Metadata && manifest.layout == StorageLayout::LegacyMigration {
            return Err(Error::RecoveryRequired(
                "legacy metadata has not been verified".into(),
            ));
        }
        manifest.volume_mut(role).state = VolumeState::Formatting;
        manifest.save(manifest_path)?;
        format(manifest.volume(role).filesystem_uuid)?;
        manifest.verify_volume(role, device)?;
    }
    manifest.volume_mut(role).state = VolumeState::Ready;
    manifest.save(manifest_path)
}

/// Returns the storage manifest path associated with a data image.
#[must_use]
pub fn manifest_path(data_image: &Path) -> PathBuf {
    data_image.with_extension("storage.json")
}
