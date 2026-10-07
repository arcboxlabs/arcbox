use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::probe::write_provision_header;
use crate::{
    Error, ImageIdentity, Result, StorageLayout, StorageManifest, VolumeRecord, VolumeRole,
    VolumeState, filesystem_uuid, manifest_path,
};

#[cfg(test)]
mod tests;

/// Verifies the manifest and the exact image pair without provisioning files.
///
/// # Errors
/// Returns an error if the paths, file identities, or filesystems do not match.
pub fn verify_pair(data: &Path, metadata: &Path) -> Result<StorageManifest> {
    let manifest = StorageManifest::load(&manifest_path(data))?;
    let directory = data
        .parent()
        .ok_or_else(|| Error::RecoveryRequired("data image has no parent".into()))?;
    if metadata.parent() != Some(directory)
        || manifest.data.filename != filename(data)?
        || manifest.metadata.filename != filename(metadata)?
    {
        return Err(Error::RecoveryRequired(
            "image paths do not match the storage manifest".into(),
        ));
    }
    manifest.verify_images(directory)?;
    Ok(manifest)
}

/// Provisions a new pair or adopts recognized existing filesystems.
///
/// An existing manifest is authoritative: a missing member is never recreated.
/// A legacy Btrfs-only image is admitted provisionally; the guest must verify
/// original metadata before it consumes the metadata image's format authority.
///
/// # Errors
/// Returns an error for incomplete pairs, unknown filesystems, or I/O failures.
pub fn prepare_pair(
    data: &Path,
    metadata: &Path,
    data_size: u64,
    metadata_size: u64,
) -> Result<StorageManifest> {
    let path = manifest_path(data);
    let directory = data
        .parent()
        .ok_or_else(|| Error::RecoveryRequired("data image has no parent".into()))?;
    if metadata.parent() != Some(directory) {
        return Err(Error::RecoveryRequired(
            "paired images must share a directory".into(),
        ));
    }
    fs::create_dir_all(directory)?;
    // Keep a stable lock inode so concurrent initializers cannot replace each other's manifest.
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.with_extension("storage.lock"))?;
    lock.lock()?;
    match fs::read(&path) {
        Ok(bytes) => {
            let manifest: StorageManifest = serde_json::from_slice(&bytes)?;
            manifest.validate()?;
            if manifest.data.filename != filename(data)?
                || manifest.metadata.filename != filename(metadata)?
            {
                return Err(Error::RecoveryRequired(
                    "image paths do not match the storage manifest".into(),
                ));
            }
            publish_images(&manifest, directory)?;
            grow_image(data, data_size)?;
            grow_image(metadata, metadata_size)?;
            return Ok(manifest);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let data_exists = data.try_exists()?;
    let metadata_exists = metadata.try_exists()?;
    let mut staged = Vec::new();
    let prepared = (|| {
        let (data_record, metadata_record, layout) = match (data_exists, metadata_exists) {
            (false, false) => (
                create_image(data, data_size, &mut staged)?,
                create_image(metadata, metadata_size, &mut staged)?,
                StorageLayout::Fresh,
            ),
            (true, true) => (
                adopt_image(data, VolumeRole::Data)?,
                adopt_image(metadata, VolumeRole::Metadata)?,
                StorageLayout::LegacyPair,
            ),
            (true, false) => (
                adopt_image(data, VolumeRole::Data)?,
                create_image(metadata, metadata_size, &mut staged)?,
                StorageLayout::LegacyMigration,
            ),
            (false, true) => {
                return Err(Error::RecoveryRequired(
                    "data image is missing while its metadata image exists".into(),
                ));
            }
        };
        let manifest = StorageManifest {
            version: 1,
            data: data_record,
            metadata: metadata_record,
            layout,
        };
        manifest.save(&path)?;
        Ok(manifest)
    })();
    let manifest = match prepared {
        Ok(manifest) => manifest,
        // The manifest may survive a crash. Its staged images must survive too.
        Err(
            error @ Error::Atomic(arcbox_atomic_file::AtomicWriteError::DurabilityUncertain {
                ..
            }),
        ) => return Err(error),
        Err(error) => {
            for path in staged {
                fs::remove_file(&path).map_err(|cleanup| {
                    Error::RecoveryRequired(format!(
                        "{error}; remove staged image {}: {cleanup}",
                        path.display()
                    ))
                })?;
            }
            return Err(error);
        }
    };
    publish_images(&manifest, directory)?;
    grow_image(data, data_size)?;
    grow_image(metadata, metadata_size)?;
    Ok(manifest)
}

fn grow_image(path: &Path, size: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    if file.metadata()?.len() < size {
        file.set_len(size)?;
        file.sync_all()?;
    }
    Ok(())
}

fn filename(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::RecoveryRequired(format!("invalid image filename {}", path.display()))
        })
}

fn staging_path(directory: &Path, uuid: Uuid) -> PathBuf {
    directory.join(format!(".arcbox-volume-{uuid}.pending"))
}

fn publish_images(manifest: &StorageManifest, directory: &Path) -> Result<()> {
    for role in [VolumeRole::Data, VolumeRole::Metadata] {
        let record = manifest.volume(role);
        let destination = directory.join(&record.filename);
        match ImageIdentity::read(&destination) {
            Ok(identity) if identity == record.image => {
                manifest.verify_volume(role, &destination)?;
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let staged = staging_path(directory, record.filesystem_uuid);
                if record.state != VolumeState::New || ImageIdentity::read(&staged)? != record.image
                {
                    return Err(Error::RecoveryRequired(format!(
                        "{} has no authorized staged image",
                        destination.display()
                    )));
                }
                manifest.verify_volume(role, &staged)?;
                // Linking cannot overwrite a foreign destination and retains the recorded inode.
                fs::hard_link(&staged, &destination)?;
            }
            Ok(_) => {
                return Err(Error::RecoveryRequired(format!(
                    "{} was replaced; restore the paired images",
                    destination.display()
                )));
            }
            Err(error) => return Err(error),
        }
    }
    let directory_file = File::open(directory)?;
    directory_file.sync_all()?;
    for record in [&manifest.data, &manifest.metadata] {
        let staged = staging_path(directory, record.filesystem_uuid);
        match ImageIdentity::read(&staged) {
            Ok(identity) if identity == record.image => fs::remove_file(staged)?,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(Error::RecoveryRequired(format!(
                    "staged image {} was replaced",
                    staged.display()
                )));
            }
            Err(error) => return Err(error),
        }
    }
    directory_file.sync_all()?;
    Ok(())
}

fn create_image(path: &Path, size: u64, staged: &mut Vec<PathBuf>) -> Result<VolumeRecord> {
    let uuid = Uuid::new_v4();
    let directory = path
        .parent()
        .ok_or_else(|| Error::RecoveryRequired("image has no parent".into()))?;
    let temporary = staging_path(directory, uuid);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    staged.push(temporary.clone());
    file.set_len(size)?;
    write_provision_header(&mut file, uuid)?;
    File::open(directory)?.sync_all()?;
    Ok(VolumeRecord {
        filename: filename(path)?,
        image: ImageIdentity::read(&temporary)?,
        filesystem_uuid: uuid,
        state: VolumeState::New,
    })
}

fn adopt_image(path: &Path, role: VolumeRole) -> Result<VolumeRecord> {
    let uuid = filesystem_uuid(path, role)?
        .filter(|uuid| !uuid.is_nil())
        .ok_or_else(|| {
            Error::RecoveryRequired(format!(
                "{} has no recognized {role:?} filesystem; an existing image cannot be initialized",
                path.display()
            ))
        })?;
    Ok(VolumeRecord {
        filename: filename(path)?,
        image: ImageIdentity::read(path)?,
        filesystem_uuid: uuid,
        state: VolumeState::Ready,
    })
}
