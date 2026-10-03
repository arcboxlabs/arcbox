//! Moving a machine out of and into the registry: `export` writes a stopped
//! machine to an archive, `import` registers a machine from one. The
//! archive format is [`archive`](super::archive); the rules a clone follows
//! (a distro machine, stopped, its data disk the one thing copied) are the
//! same here and live in [`clone`](super::clone).

use std::path::{Path, PathBuf};

use arcbox_image::machine_image::MachineImageManifest;
use chrono::Utc;

use super::archive::{self, ArchiveManifest, ArchivedMachine, FORMAT_VERSION};
use super::clone::{DATA_DISK, stopped_data_disk};
use super::{MachineConfig, MachineInfo, MachineManager, clone_file, reserve_hostname};
use crate::error::{EngineError, Result};

/// Prefix of the staging directory an export snapshots its data disk into.
pub(super) const EXPORT_STAGING_PREFIX: &str = ".export-";
/// Prefix of the staging directory an import restores its data disk into.
pub(super) const IMPORT_STAGING_PREFIX: &str = ".import-";

/// Where a new machine's data disk comes from.
pub(super) enum DataDisk {
    /// A fresh sparse image of the configured size.
    Sparse,
    /// A restored image at this path, moved into the machine directory.
    Staged(PathBuf),
}

impl MachineManager {
    /// Writes machine `name` to the archive at `path`: its settings, the
    /// manifest of the published `image` it boots, and its data disk.
    /// Returns the archive's size in bytes.
    ///
    /// The data disk is snapshotted (a copy-on-write clone) under the
    /// registry lock, so the archive is a consistent image of the machine
    /// as it was at that instant, and the archive is written from the
    /// snapshot without holding the lock.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine is unknown, is not a stopped distro
    /// machine, or the archive cannot be written.
    pub fn export(&self, name: &str, path: &Path, image: MachineImageManifest) -> Result<u64> {
        let (snapshot, machine) = {
            let machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;
            let machine = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_owned()))?;
            let disk = stopped_data_disk(machine)?;
            std::fs::create_dir_all(&self.machines_dir)?;
            let snapshot = tempfile::Builder::new()
                .prefix(EXPORT_STAGING_PREFIX)
                .tempdir_in(&self.machines_dir)?;
            clone_file(&disk, &snapshot.path().join(DATA_DISK))?;
            (snapshot, ArchivedMachine::from(machine))
        };
        let manifest = ArchiveManifest {
            format_version: FORMAT_VERSION,
            arcbox_version: env!("CARGO_PKG_VERSION").to_owned(),
            exported_at: Utc::now(),
            machine,
            image,
        };
        let size = archive::write(path, &manifest, &snapshot.path().join(DATA_DISK))?;
        tracing::info!(machine = name, path = %path.display(), size, "exported machine");
        Ok(size)
    }

    /// Registers a machine from the archive at `path`, configured by
    /// `config` — the caller's reading of the archive's manifest, with the
    /// rootfs resolved against the local image registry.
    ///
    /// The data disk is restored into a staging directory first, so the
    /// registry lock is held only to move it into place and register the
    /// machine, as `create` does.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is taken or cannot be a hostname, the
    /// archive cannot be read, or the machine cannot be created.
    pub fn import(&self, config: MachineConfig, path: &Path) -> Result<String> {
        self.ensure_name_available(&config.name)?;
        std::fs::create_dir_all(&self.machines_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(IMPORT_STAGING_PREFIX)
            .tempdir_in(&self.machines_dir)?;
        let staged = staging.path().join(DATA_DISK);
        archive::extract_data_disk(path, &staged)?;
        let name = self.create_machine(config, DataDisk::Staged(staged))?;
        tracing::info!(machine = %name, path = %path.display(), "imported machine");
        Ok(name)
    }

    /// Checks that `name` can be registered: unused, and not another
    /// machine's hostname. `create`, `clone_machine` and `import` check
    /// again under the lock they insert under; this is for a caller that
    /// wants to fail before doing expensive work.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is taken or cannot be a hostname.
    pub fn ensure_name_available(&self, name: &str) -> Result<()> {
        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;
        reserve_hostname(&machines, name).map(|_| ())
    }

    /// Removes the staging directories of an export or import a previous
    /// daemon did not finish.
    pub(super) fn sweep_staging(machines_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(machines_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(EXPORT_STAGING_PREFIX) || name.starts_with(IMPORT_STAGING_PREFIX))
            {
                continue;
            }
            let path = entry.path();
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "removed a stale staging directory");
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not remove a stale staging directory");
                }
            }
        }
    }
}

impl From<&MachineInfo> for ArchivedMachine {
    fn from(machine: &MachineInfo) -> Self {
        Self {
            name: machine.name.clone(),
            cpus: machine.cpus,
            memory_mb: machine.memory_mb,
            disk_gb: machine.disk_gb,
            distro: machine.distro.clone().unwrap_or_default(),
            distro_version: machine.distro_version.clone(),
            mounts: machine.mounts.clone(),
        }
    }
}
