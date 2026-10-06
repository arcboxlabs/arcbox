//! Retains the recovery control plane after explicit storage startup failures.

use std::{fs, path::Path};

use arcbox_connect::v1::{
    RuntimeStatusResponse, StorageRecoveryProgress,
    storage_recovery_progress::Phase,
    storage_volume_health::{Role, State},
};

use crate::{
    Runtime,
    error::{CoreError, Result},
};

impl Runtime {
    /// Publishes a storage validation failure only after the boot path created a hold.
    pub(in crate::runtime) fn protect_failed_storage_boot(
        &self,
        error: &arcbox_engine::EngineError,
    ) -> Result<bool> {
        let operation_id = match fs::read_to_string(self.machine_manager.storage_hold_path()) {
            Ok(operation_id) => operation_id,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        self.persist_storage_protection(StorageRecoveryProgress {
            operation_id,
            phase: Phase::Failed.into(),
            message: error.to_string(),
            storage_protected: true,
            ..Default::default()
        })?;
        Ok(true)
    }

    /// Checks the current guest directly before classifying a runtime startup failure.
    pub(in crate::runtime) async fn protect_failed_runtime_start(
        &self,
        error: &CoreError,
    ) -> Result<bool> {
        let status = match self.probe_startup_runtime_status().await {
            Ok(status) => status,
            Err(probe) => {
                tracing::warn!(%probe, "Cannot inspect storage after runtime startup failure");
                return Ok(false);
            }
        };
        self.protect_observed_runtime_failure(error, &status).await
    }

    async fn probe_startup_runtime_status(&self) -> Result<RuntimeStatusResponse> {
        let mut agent = self.connect_system_agent().await?;
        if agent.is_blocking() {
            tokio::task::spawn_blocking(move || agent.get_runtime_status_blocking())
                .await
                .map_err(|error| CoreError::Vm(format!("startup storage probe task: {error}")))?
                .map_err(CoreError::from)
        } else {
            agent.get_runtime_status().await.map_err(CoreError::from)
        }
    }

    async fn protect_observed_runtime_failure(
        &self,
        error: &CoreError,
        status: &RuntimeStatusResponse,
    ) -> Result<bool> {
        let faults = status
            .storage_health
            .volumes
            .iter()
            .filter(|volume| {
                matches!(volume.role.as_known(), Some(Role::Data | Role::Metadata))
                    && matches!(
                        volume.state.as_known(),
                        Some(State::ReadOnly | State::Unavailable)
                    )
            })
            .map(|volume| format!("{:?}={:?}", volume.role, volume.state))
            .collect::<Vec<_>>();
        if faults.is_empty() {
            return Ok(false);
        }

        let operation_id = uuid::Uuid::new_v4().to_string();
        let progress = StorageRecoveryProgress {
            phase: Phase::Failed.into(),
            message: format!(
                "Runtime startup failed while required storage was unavailable for writes ({}): {error}",
                faults.join(", ")
            ),
            recovery_directory: self
                .storage_recovery
                .directory
                .join(&operation_id)
                .to_string_lossy()
                .into_owned(),
            operation_id,
            storage_protected: true,
            ..Default::default()
        };
        let preserved = self
            .persist_storage_protection(progress.clone())
            .and_then(|()| {
                fs::create_dir(&progress.recovery_directory)?;
                fs::File::open(&self.storage_recovery.directory)?.sync_all()?;
                arcbox_atomic_file::write(
                    &Path::new(&progress.recovery_directory).join("startup-runtime-status.txt"),
                    format!("{}\n\n{status:#?}\n", progress.message).as_bytes(),
                )?;
                Ok(())
            });
        // A persistence error must not skip stopping the guest with unusable storage.
        let stopped = async {
            let reservation = self.storage_recovery.owner.reserve(&self.machine_manager)?;
            self.vm_lifecycle
                .stop_storage(&reservation)
                .await
                .map_err(CoreError::from)
        }
        .await;
        if preserved.is_err() || stopped.is_err() {
            return Err(CoreError::Machine(format!(
                "{error}; preserving storage startup diagnostics: {preserved:?}; stopping the System VM: {stopped:?}"
            )));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use arcbox_connect::v1::{StorageHealth, StorageVolumeHealth};

    use super::*;
    use crate::runtime::storage_recovery::StorageRecovery;

    fn runtime(directory: &Path) -> Runtime {
        Runtime::new(crate::config::Config {
            data_dir: directory.to_owned(),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn startup_storage_protection_requires_a_hold_and_preserves_the_cause() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(directory.path());
        let generic = arcbox_engine::EngineError::Vm("agent handshake timed out".into());
        assert!(!runtime.protect_failed_storage_boot(&generic).unwrap());
        assert!(!runtime.storage_writes_protected());
        assert!(runtime.subscribe_storage_recovery().0.is_none());

        fs::create_dir(directory.path().join("storage-recovery")).unwrap();
        arcbox_atomic_file::write(
            &runtime.machine_manager.storage_hold_path(),
            b"startup-check",
        )
        .unwrap();
        let failure = arcbox_engine::EngineError::config("metadata image is missing");
        assert!(runtime.protect_failed_storage_boot(&failure).unwrap());
        let progress = runtime.subscribe_storage_recovery().0.unwrap();
        assert_eq!(progress.phase, Phase::Failed);
        assert_eq!(progress.operation_id, "startup-check");
        assert_eq!(progress.message, failure.to_string());
        assert!(progress.storage_protected);
        assert!(runtime.storage_writes_protected());

        let restored = StorageRecovery::load(directory.path()).unwrap();
        assert_eq!(restored.subscribe().0.unwrap(), progress);
    }

    #[tokio::test]
    async fn only_explicit_required_volume_faults_create_startup_protection() {
        for role in [Role::RoleUnspecified, Role::Data, Role::Metadata] {
            for state in [
                State::StateUnspecified,
                State::MountedReadWrite,
                State::ReadOnly,
                State::Unavailable,
                State::NotConfigured,
            ] {
                let directory = tempfile::tempdir().unwrap();
                let runtime = runtime(directory.path());
                let status = RuntimeStatusResponse {
                    storage_health: Some(StorageHealth {
                        volumes: vec![StorageVolumeHealth {
                            role: role.into(),
                            state: state.into(),
                            device: "/dev/vdb".into(),
                            mount_point: "/run/arcbox/data".into(),
                            detail: "fresh guest observation".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };
                let failure = CoreError::Machine("guest mount failed".into());
                let protected = runtime
                    .protect_observed_runtime_failure(&failure, &status)
                    .await
                    .unwrap();
                let expected = matches!(role, Role::Data | Role::Metadata)
                    && matches!(state, State::ReadOnly | State::Unavailable);
                assert_eq!(protected, expected);
                assert_eq!(runtime.storage_writes_protected(), expected);
                assert_eq!(
                    runtime.machine_manager.storage_hold_path().exists(),
                    expected
                );
                if expected {
                    let progress = runtime.subscribe_storage_recovery().0.unwrap();
                    assert_eq!(progress.phase, Phase::Failed);
                    assert!(progress.storage_protected);
                    assert!(progress.message.contains(&failure.to_string()));
                    let diagnostic = fs::read_to_string(
                        Path::new(&progress.recovery_directory).join("startup-runtime-status.txt"),
                    )
                    .unwrap();
                    assert!(diagnostic.contains("fresh guest observation"));
                    assert!(diagnostic.contains("/dev/vdb"));
                    assert_eq!(
                        StorageRecovery::load(directory.path())
                            .unwrap()
                            .subscribe()
                            .0
                            .unwrap(),
                        progress
                    );
                } else {
                    assert!(runtime.subscribe_storage_recovery().0.is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn startup_persistence_failure_reuses_protection_to_stop_the_vm() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(directory.path());
        fs::write(&runtime.storage_recovery.directory, b"not a directory").unwrap();
        let status = RuntimeStatusResponse {
            storage_health: Some(StorageHealth {
                volumes: vec![StorageVolumeHealth {
                    role: Role::Data.into(),
                    state: State::ReadOnly.into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runtime.protect_observed_runtime_failure(
                &CoreError::Machine("guest mount failed".into()),
                &status,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("stopping the System VM: Ok(())"));
        fs::remove_file(&runtime.storage_recovery.directory).unwrap();
        assert!(
            !runtime
                .machine_manager
                .storage_hold_path()
                .try_exists()
                .unwrap()
        );
        assert!(runtime.machine_manager.storage_is_held().unwrap());
        assert!(runtime.storage_writes_protected());
    }

    #[tokio::test]
    async fn failed_fresh_probe_does_not_reclassify_startup() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(directory.path());
        let failure = CoreError::Machine("runtime startup failed".into());
        assert!(
            !runtime
                .protect_failed_runtime_start(&failure)
                .await
                .unwrap()
        );
        assert!(!runtime.storage_writes_protected());
        assert!(runtime.subscribe_storage_recovery().0.is_none());
    }
}
