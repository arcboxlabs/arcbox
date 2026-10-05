//! Guest access to the host-owned, durable volume initialization contract.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use arcbox_storage::{StorageLayout, StorageManifest, VolumeRole};
use uuid::Uuid;

static MANIFEST_LOCK: Mutex<()> = Mutex::new(());

fn manifest_path() -> Result<PathBuf, String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline")
        .map_err(|error| format!("read storage manifest declaration: {error}"))?;
    cmdline.split_whitespace()
        .find_map(|token| token.strip_prefix(arcbox_storage::MANIFEST_CMDLINE_KEY))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "runtime storage needs recovery: no storage manifest was declared; automatic formatting is forbidden".into())
}

pub(super) fn ensure_filesystem(
    role: VolumeRole,
    device: &str,
    format: impl FnOnce(Uuid) -> Result<(), String>,
) -> Result<(), String> {
    let _guard = MANIFEST_LOCK
        .lock()
        .map_err(|_| "storage manifest lock poisoned".to_string())?;
    arcbox_storage::ensure_filesystem(&manifest_path()?, role, Path::new(device), |uuid| {
        format(uuid).map_err(arcbox_storage::Error::RecoveryRequired)
    })
    .map_err(|error| error.to_string())
}

pub(super) fn authorize_legacy_migration(
    verify_sources: impl FnOnce() -> std::io::Result<()>,
) -> Result<bool, String> {
    let _guard = MANIFEST_LOCK
        .lock()
        .map_err(|_| "storage manifest lock poisoned".to_string())?;
    let path = manifest_path()?;
    let mut manifest = StorageManifest::load(&path).map_err(|error| error.to_string())?;
    if manifest.layout == StorageLayout::LegacyMigration {
        verify_sources().map_err(|error| {
            format!("runtime storage needs recovery: legacy metadata cannot be verified: {error}")
        })?;
        manifest.layout = StorageLayout::Migrating;
        manifest.save(&path).map_err(|error| error.to_string())?;
    }
    Ok(manifest.layout == StorageLayout::Paired)
}

pub(super) fn finish_metadata_setup() -> Result<(), String> {
    let _guard = MANIFEST_LOCK
        .lock()
        .map_err(|_| "storage manifest lock poisoned".to_string())?;
    let path = manifest_path()?;
    let mut manifest = StorageManifest::load(&path).map_err(|error| error.to_string())?;
    if manifest.layout != StorageLayout::Paired {
        manifest.layout = StorageLayout::Paired;
        manifest.save(&path).map_err(|error| error.to_string())?;
    }
    Ok(())
}
