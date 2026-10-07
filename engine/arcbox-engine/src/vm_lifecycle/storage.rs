use crate::error::{EngineError, Result};
use crate::machine::{MachineInfo, StorageMaintenance};

use super::VmLifecycleManager;

impl VmLifecycleManager {
    /// Rebuilds a missing System VM record from verified assets and its existing pair.
    /// The caller must retain the reservation and stop the VM before copying disks.
    pub async fn storage_recovery_machine(
        &self,
        reservation: &StorageMaintenance,
    ) -> Result<MachineInfo> {
        self.check_storage_maintenance(reservation).await?;
        if let Some(machine) = self.shared.machine_manager.get(&self.shared.machine_name) {
            return Ok(machine);
        }
        let assets = self
            .shared
            .boot_assets
            .verified_cached_assets()
            .await
            .map_err(|error| {
                EngineError::config(format!(
                    "cannot rebuild System VM storage configuration: {error}"
                ))
            })?;
        let config = self
            .shared
            .default_machine_config(self.shared.desired_boot(assets));
        let reservation = reservation.clone();
        tokio::task::spawn_blocking(move || reservation.create_system_machine(config))
            .await
            .map_err(|error| {
                EngineError::Machine(format!("restore System VM configuration: {error}"))
            })??;
        self.shared
            .machine_manager
            .get(&self.shared.machine_name)
            .ok_or_else(|| EngineError::not_found("System VM storage configuration"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::MachineState;
    use arcbox_image::boot_assets::{BootAssetConfig, BootAssetProvider};
    use std::{fs, os::unix::fs::MetadataExt, path::Path, sync::Arc};

    fn fixture(directory: &Path) -> VmLifecycleManager {
        let (_, mut lifecycle) = super::super::tests::storage_test_lifecycle(directory);
        let config = BootAssetConfig::with_cache_dir(directory.join("boot"))
            .with_version("0.0.0")
            .with_unpinned_manifest_allowed(true);
        let cache = config.version_cache_dir();
        fs::create_dir_all(&cache).unwrap();
        // SHA256 of the one-byte fixture in both boot images.
        let hash = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
        fs::write(
            cache.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 0, "asset_version": config.version, "built_at": "fixture",
                "targets": {&config.arch: {
                    "kernel": {"path": "kernel", "sha256": hash},
                    "rootfs": {"path": "rootfs", "sha256": hash},
                    "kernel_cmdline": "root=/dev/vda ro rootfstype=erofs"
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        for name in [
            "kernel",
            "rootfs.erofs",
            "runtime.erofs",
            "verify-cache.json",
        ] {
            fs::write(cache.join(name), b"a").unwrap();
        }
        Arc::get_mut(&mut lifecycle.shared).unwrap().boot_assets =
            Arc::new(BootAssetProvider::with_config(config).unwrap());
        arcbox_storage::prepare_pair(
            &directory.join("data/docker.img"),
            &directory.join("data/docker-meta.img"),
            4096,
            4096,
        )
        .unwrap();
        fs::create_dir(directory.join("storage-recovery")).unwrap();
        fs::write(
            lifecycle.shared.machine_manager.storage_hold_path(),
            b"held",
        )
        .unwrap();
        lifecycle
    }

    fn snapshot(directory: &Path) -> Vec<(String, Vec<u8>, u64)> {
        ["data", "boot/0.0.0", "storage-recovery"]
            .into_iter()
            .flat_map(|subdir| fs::read_dir(directory.join(subdir)).unwrap())
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.to_string_lossy().into_owned(),
                    fs::read(&path).unwrap(),
                    fs::metadata(path).unwrap().ino(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn reconstructs_only_the_missing_record_and_keeps_existing_records_inspectable() {
        let directory = tempfile::tempdir().unwrap();
        let lifecycle = fixture(directory.path());
        let machines = &lifecycle.shared.machine_manager;
        let reservation = machines.reserve_storage().unwrap();
        let original = snapshot(directory.path());
        let machine = lifecycle
            .storage_recovery_machine(&reservation)
            .await
            .unwrap();
        assert_eq!(machine.state, MachineState::Created);
        assert_eq!(machine.block_devices.len(), 3);
        assert_eq!(snapshot(directory.path()), original);
        assert!(
            directory
                .path()
                .join("machines/default/config.toml")
                .is_file()
        );
        assert!(machines.ensure_storage_available(&machine.name).is_err());
        assert!(matches!(
            lifecycle.state().await,
            super::super::VmLifecycleState::NotExist
        ));

        fs::remove_file(directory.path().join("boot/0.0.0/kernel")).unwrap();
        fs::write(directory.path().join("data/docker-meta.img"), b"corrupt").unwrap();
        let original = snapshot(directory.path());
        let existing = lifecycle
            .storage_recovery_machine(&reservation)
            .await
            .unwrap();
        assert_eq!(existing.vm_id, machine.vm_id);
        assert_eq!(snapshot(directory.path()), original);
    }

    #[tokio::test]
    async fn refuses_missing_or_invalid_inputs_without_creating_a_record_or_modifying_files() {
        for (file, remove) in [
            ("data/docker.img", true),
            ("data/docker-meta.img", true),
            ("data/docker-meta.img", false),
            ("boot/0.0.0/manifest.json", true),
            ("boot/0.0.0/manifest.json", false),
            ("boot/0.0.0/kernel", true),
            ("boot/0.0.0/kernel", false),
            ("boot/0.0.0/rootfs.erofs", true),
            ("boot/0.0.0/rootfs.erofs", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let lifecycle = fixture(directory.path());
            if remove {
                fs::remove_file(directory.path().join(file)).unwrap();
            } else {
                fs::write(directory.path().join(file), b"corrupt").unwrap();
            }
            let machines = &lifecycle.shared.machine_manager;
            let reservation = machines.reserve_storage().unwrap();
            let original = snapshot(directory.path());
            assert!(
                lifecycle
                    .storage_recovery_machine(&reservation)
                    .await
                    .is_err(),
                "{file}"
            );
            assert!(machines.get("default").is_none());
            assert!(
                !directory
                    .path()
                    .join("machines/default/config.toml")
                    .exists()
            );
            assert_eq!(snapshot(directory.path()), original);
            assert!(machines.storage_is_held().unwrap());
        }
    }
}
