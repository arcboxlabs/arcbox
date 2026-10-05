use std::fs::{self, File, OpenOptions};
use std::path::Path;

use uuid::Uuid;

use crate::probe::write_provision_header;
use crate::{
    Error, ImageIdentity, Result, StorageLayout, StorageManifest, VolumeRecord, VolumeRole,
    VolumeState, filesystem_uuid, manifest_path,
};

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
            manifest.verify_images(directory)?;
            grow_image(data, data_size)?;
            grow_image(metadata, metadata_size)?;
            return Ok(manifest);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let data_exists = data.try_exists()?;
    let metadata_exists = metadata.try_exists()?;
    fs::create_dir_all(directory)?;
    let (data_record, metadata_record, layout) = match (data_exists, metadata_exists) {
        (false, false) => (
            create_image(data, data_size)?,
            create_image(metadata, metadata_size)?,
            StorageLayout::Fresh,
        ),
        (true, true) => (
            adopt_image(data, VolumeRole::Data)?,
            adopt_image(metadata, VolumeRole::Metadata)?,
            StorageLayout::Paired,
        ),
        (true, false) => (
            adopt_image(data, VolumeRole::Data)?,
            create_image(metadata, metadata_size)?,
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

fn create_image(path: &Path, size: u64) -> Result<VolumeRecord> {
    let uuid = Uuid::new_v4();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.set_len(size)?;
    write_provision_header(&mut file, uuid)?;
    File::open(
        path.parent()
            .ok_or_else(|| Error::RecoveryRequired("image has no parent".into()))?,
    )?
    .sync_all()?;
    Ok(VolumeRecord {
        filename: filename(path)?,
        image: ImageIdentity::read(path)?,
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
