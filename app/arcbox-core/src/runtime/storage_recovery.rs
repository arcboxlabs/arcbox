//! Daemon-owned storage checks with preserved disks and explicit restart verification.

mod guest;
mod owner;
mod preserve;
mod startup;
mod state;

#[cfg(test)]
mod admission_tests;

use crate::{
    Runtime,
    error::{CoreError, Result},
};
use arcbox_connect::v1::{
    StorageRecoveryProgress, recover_storage_request::Action, storage_recovery_progress::Phase,
};
use arcbox_engine::machine::{DEFAULT_MACHINE_NAME, MachineState};
pub(super) use state::StorageRecovery;
use std::{
    fmt::Write as _,
    fs,
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::broadcast;

impl Runtime {
    /// Reports whether a daemon-owned storage operation is executing.
    #[must_use]
    pub fn storage_recovery_active(&self) -> bool {
        self.storage_recovery.active.load(Ordering::Acquire)
    }

    /// Retains write protection after failed or check-only recovery, including a failed stop.
    #[must_use]
    pub fn storage_writes_protected(&self) -> bool {
        self.storage_recovery.protected.load(Ordering::Acquire) || self.storage_recovery_active()
    }

    /// Returns the last recovery result and all subsequent progress updates.
    pub fn subscribe_storage_recovery(
        &self,
    ) -> (
        Option<StorageRecoveryProgress>,
        broadcast::Receiver<StorageRecoveryProgress>,
    ) {
        self.storage_recovery.subscribe()
    }

    /// Starts one recovery operation. Client disconnects do not cancel the operation.
    /// Both actions stop workloads and preserve the paired images before checking.
    pub async fn recover_storage(
        self: &Arc<Self>,
        action: Action,
    ) -> Result<(
        StorageRecoveryProgress,
        broadcast::Receiver<StorageRecoveryProgress>,
    )> {
        if !matches!(action, Action::CheckOnly | Action::Recover) {
            return Err(CoreError::config("action must be CHECK_ONLY or RECOVER"));
        }
        let owner = &self.storage_recovery.owner;
        let mut tasks = owner.tasks.lock().await;
        owner.check_open()?;
        let operation = Arc::clone(&self.storage_recovery.operation)
            .try_lock_owned()
            .map_err(|_| CoreError::invalid_state("another System VM operation is running"))?;
        tasks.join().await;
        if let Some(error) = &tasks.join_error {
            let held = self.retain_worker_failure(error);
            return Err(CoreError::Machine(format!(
                "previous recovery worker: {error}; preserving recovery hold: {held:?}"
            )));
        }
        if let Some(error) = &tasks.cleanup_error {
            return Err(CoreError::Machine(format!(
                "previous recovery VM cleanup failed: {error}"
            )));
        }
        let migration = self.migration_manager.reserve_recovery().await?;
        let reservation = owner.reserve(&self.machine_manager)?;
        self.vm_lifecycle
            .check_storage_maintenance(&reservation)
            .await?;
        owner.check_open()?;
        let directory = &self.storage_recovery.directory;
        let operation_id = uuid::Uuid::new_v4().to_string();
        let progress = StorageRecoveryProgress {
            phase: Phase::Stopping.into(),
            message: "Stopping workloads before preserving the paired disks".into(),
            recovery_directory: directory.join(&operation_id).to_string_lossy().into_owned(),
            operation_id,
            storage_protected: true,
            ..Default::default()
        };
        owner.retain(reservation.clone());
        self.persist_storage_protection(progress.clone())?;
        let (_, updates) = self.storage_recovery.subscribe();
        self.storage_recovery.active.store(true, Ordering::Release);
        let runtime = Arc::clone(self);
        let initial = progress.clone();
        tasks.worker = Some(tokio::spawn(async move {
            let _operation = operation;
            let _migration = migration;
            let _active = ActiveRecovery(Arc::clone(&runtime));
            let mut progress = progress;
            let mut cleanup = Ok(());
            if let Err(error) = runtime
                .run_storage_recovery(action, reservation, &mut progress, &mut cleanup)
                .await
            {
                progress.phase = Phase::Failed.into();
                progress.storage_protected = true;
                progress.message = error.to_string();
                if let Err(persist) = runtime.storage_recovery.publish(progress.clone()) {
                    let _ = write!(
                        progress.message,
                        "; could not persist recovery outcome: {persist}"
                    );
                    runtime.storage_recovery.publish_observation(progress);
                }
            }
            cleanup
        }));
        Ok((initial, updates))
    }

    fn persist_storage_protection(&self, progress: StorageRecoveryProgress) -> Result<()> {
        // A daemon crash must not turn an interrupted check into an automatic boot.
        self.storage_recovery
            .protected
            .store(true, Ordering::Release);
        let persisted = (|| -> Result<()> {
            let owner = &self.storage_recovery.owner;
            owner.retain(owner.reserve(&self.machine_manager)?);
            fs::create_dir_all(&self.storage_recovery.directory)?;
            fs::File::open(&self.config.data_dir)?.sync_all()?;
            arcbox_atomic_file::write(
                &self.machine_manager.storage_hold_path(),
                progress.operation_id.as_bytes(),
            )?;
            self.storage_recovery.publish(progress.clone())
        })();
        if let Err(error) = persisted {
            let mut failed = progress;
            failed.phase = Phase::Failed.into();
            let _ = write!(
                failed.message,
                "; could not persist storage protection: {error}"
            );
            self.storage_recovery.publish_observation(failed);
            return Err(error);
        }
        Ok(())
    }

    async fn run_storage_recovery(
        self: &Arc<Self>,
        action: Action,
        reservation: arcbox_engine::machine::StorageMaintenance,
        progress: &mut StorageRecoveryProgress,
        cleanup: &mut Result<()>,
    ) -> Result<()> {
        self.stop_recovery_vm(&reservation).await?;
        self.storage_recovery.owner.check_open()?;
        let machine = self
            .machine_manager
            .get(DEFAULT_MACHINE_NAME)
            .ok_or_else(|| CoreError::not_found("System VM storage configuration"))?;
        if !matches!(machine.state, MachineState::Created | MachineState::Stopped) {
            return Err(CoreError::invalid_state(
                "System VM did not stop; no disk copy was attempted",
            ));
        }
        self.recovery_phase(
            progress,
            Phase::Preserving,
            "Preserving both disks and their manifest",
        )?;
        let copies = {
            let machine = machine.clone();
            let directory = std::path::PathBuf::from(&progress.recovery_directory);
            tokio::task::spawn_blocking(move || preserve::pair(&machine, &directory))
                .await
                .map_err(|error| CoreError::Machine(format!("preserve storage task: {error}")))??
        };
        self.storage_recovery.owner.check_open()?;
        self.recovery_phase(
            progress,
            Phase::Checking,
            "Checking unmounted recovery copies without filesystem repair",
        )?;
        let offline = guest::offline_check(self, &machine, copies, progress).await?;
        *cleanup = offline.cleaned;
        match (&offline.checked, &cleanup) {
            (Ok(()), Ok(())) => {}
            (checked, cleaned) => {
                return Err(CoreError::Machine(format!(
                    "offline storage check: {checked:?}; recovery VM cleanup: {cleaned:?}"
                )));
            }
        }
        self.storage_recovery.owner.check_open()?;
        if action == Action::CheckOnly {
            self.recovery_phase(progress, Phase::Complete, "Offline checks passed. The System VM remains stopped; choose Recover to restart and verify writes.")?;
            return Ok(());
        }
        // The check uses copies; verify that the original attachments still identify the same pair.
        let [_, data, metadata, ..] = machine.block_devices.as_slice() else {
            return Err(CoreError::invalid_state(
                "paired disk configuration is missing",
            ));
        };
        arcbox_storage::verify_pair(data.path.as_ref(), metadata.path.as_ref())?;
        self.recovery_phase(
            progress,
            Phase::Restarting,
            "Offline checks passed; restarting the System VM",
        )?;
        let verification = async {
            self.vm_lifecycle
                .resume_storage(&reservation, self.storage_recovery.owner.cancelled.clone())
                .await?;
            guest::wait_runtime(self).await?;
            self.recovery_phase(
                progress,
                Phase::Verifying,
                "Verifying durable writes on both disks and a Docker container lifecycle",
            )?;
            guest::verify_writes(self, progress).await?;
            self.storage_recovery.owner.complete(|| {
                fs::remove_file(self.machine_manager.storage_hold_path())?;
                fs::File::open(&self.storage_recovery.directory)?.sync_all()?;
                progress.storage_protected = false;
                self.recovery_phase(
                    progress,
                    Phase::Complete,
                    "Storage checks, durable writes, and the Docker container lifecycle passed",
                )
            })
        }
        .await;
        if let Err(error) = verification {
            // Reinstate the durable hold before stopping; no caller may race a new boot.
            let hold = arcbox_atomic_file::write(
                &self.machine_manager.storage_hold_path(),
                progress.operation_id.as_bytes(),
            );
            let stopped = self.stop_recovery_vm(&reservation).await;
            return Err(CoreError::Machine(format!(
                "{error}; recovery boot hold: {hold:?}; stopping unverified runtime: {stopped:?}"
            )));
        }
        self.storage_recovery
            .protected
            .store(false, Ordering::Release);
        Ok(())
    }

    async fn stop_recovery_vm(
        &self,
        reservation: &arcbox_engine::machine::StorageMaintenance,
    ) -> Result<()> {
        tokio::select! {
            biased;
            () = self.storage_recovery.owner.cancelled.cancelled() => {
                // The actor retains the graceful-stop task and joins it before the reserved stop.
                self.vm_lifecycle.stop_storage(reservation).await?;
            }
            stopped = self.vm_lifecycle.shutdown() => stopped?,
        }
        Ok(())
    }

    pub(super) async fn close_storage_recovery(&self) -> Result<bool> {
        let owner = &self.storage_recovery.owner;
        // Cancel before the mutex: a second daemon signal can interrupt the first join.
        owner.close();
        let mut tasks = owner.tasks.lock().await;
        tasks.join().await;
        let retained = tasks
            .join_error
            .as_ref()
            .map(|error| self.retain_worker_failure(error));
        let protection = if self.storage_writes_protected() {
            Ok(true)
        } else {
            self.machine_manager.storage_is_held()
        };
        // An unreadable hold must not permit configuration removal during shutdown.
        let protected = *protection.as_ref().unwrap_or(&true);
        let stopped = async {
            if protected {
                let reservation = match owner.reserve(&self.machine_manager) {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        let stopped = self.vm_lifecycle.shutdown().await;
                        return Err(CoreError::Machine(format!("reserving storage for shutdown: {error}; lifecycle shutdown: {stopped:?}")));
                    }
                };
                // Shutdown must not reopen admission when durable protection could not be written.
                owner.retain(reservation.clone());
                self.vm_lifecycle.stop_storage(&reservation).await?;
            }
            Ok::<_, CoreError>(protected)
        }
        .await;
        match (&tasks.join_error, &tasks.cleanup_error, protection, stopped) {
            (None, None, Ok(_), Ok(protected)) => Ok(protected),
            (join, cleanup, protection, stopped) => Err(CoreError::Machine(format!(
                "recovery worker: {join:?}; recovery VM cleanup: {cleanup:?}; preserving recovery hold: {retained:?}; reading storage hold: {protection:?}; stopping reserved System VM: {stopped:?}"
            ))),
        }
    }

    fn retain_worker_failure(&self, error: &str) -> Result<()> {
        let (progress, _) = self.storage_recovery.subscribe();
        let mut progress = progress.ok_or_else(|| {
            CoreError::invalid_state("recovery worker has no durable operation record")
        })?;
        progress.phase = Phase::Failed.into();
        progress.storage_protected = true;
        progress.message = format!("Recovery worker failed; the outcome is unverified: {error}");
        self.persist_storage_protection(progress)
    }

    fn recovery_phase(
        &self,
        progress: &mut StorageRecoveryProgress,
        phase: Phase,
        message: &str,
    ) -> Result<()> {
        progress.phase = phase.into();
        message.clone_into(&mut progress.message);
        self.storage_recovery.publish(progress.clone())
    }
}

struct ActiveRecovery(Arc<Runtime>);

impl Drop for ActiveRecovery {
    fn drop(&mut self) {
        self.0
            .storage_recovery
            .active
            .store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_reconciles_interrupted_journal_before_system_vm_admission() {
        let directory = tempfile::tempdir().unwrap();
        let state = StorageRecovery::load(directory.path()).unwrap();
        std::fs::create_dir(&state.directory).unwrap();
        state
            .publish(StorageRecoveryProgress {
                operation_id: "interrupted-runtime-check".into(),
                phase: Phase::Verifying.into(),
                recovery_directory: "/preserved/pair".into(),
                ..Default::default()
            })
            .unwrap();
        drop(state);

        let runtime = Runtime::new(crate::config::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        let progress = runtime.subscribe_storage_recovery().0.unwrap();
        assert_eq!(progress.phase, Phase::Failed);
        assert_eq!(progress.operation_id, "interrupted-runtime-check");
        assert_eq!(progress.recovery_directory, "/preserved/pair");
        assert!(progress.storage_protected);
        assert!(runtime.storage_writes_protected());
        assert!(
            runtime
                .machine_manager
                .ensure_storage_available(arcbox_engine::machine::DEFAULT_MACHINE_NAME)
                .is_err()
        );
        assert_eq!(
            std::fs::read(runtime.machine_manager.storage_hold_path()).unwrap(),
            b"interrupted-runtime-check"
        );
    }

    #[tokio::test]
    async fn a_previous_worker_panic_retains_protection_and_blocks_the_next_operation() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            Runtime::new(crate::config::Config {
                data_dir: directory.path().to_owned(),
                ..Default::default()
            })
            .unwrap(),
        );
        runtime
            .persist_storage_protection(StorageRecoveryProgress {
                operation_id: "interrupted".into(),
                storage_protected: true,
                ..Default::default()
            })
            .unwrap();
        runtime.storage_recovery.owner.tasks.lock().await.worker = Some(tokio::spawn(async {
            panic!("recovery worker failed");
        }));
        let error = runtime
            .recover_storage(Action::CheckOnly)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("recovery worker failed"));
        assert!(runtime.machine_manager.storage_hold_path().is_file());
        assert!(runtime.storage_writes_protected());
        assert_eq!(
            runtime.subscribe_storage_recovery().0.unwrap().phase,
            Phase::Failed
        );
    }

    #[tokio::test]
    async fn shutdown_reports_disposable_vm_cleanup_failure_and_keeps_the_hold() {
        for cleanup_fails in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let runtime = Runtime::new(crate::config::Config {
                data_dir: directory.path().to_owned(),
                ..Default::default()
            })
            .unwrap();
            runtime
                .persist_storage_protection(StorageRecoveryProgress {
                    operation_id: "cancelled".into(),
                    storage_protected: true,
                    ..Default::default()
                })
                .unwrap();
            runtime.storage_recovery.owner.tasks.lock().await.worker =
                Some(tokio::spawn(async move {
                    if cleanup_fails {
                        Err(CoreError::Machine("disposable VM removal failed".into()))
                    } else {
                        Ok(())
                    }
                }));
            let closed = runtime.close_storage_recovery().await;
            assert_eq!(closed.is_err(), cleanup_fails);
            if let Err(error) = closed {
                assert!(error.to_string().contains("disposable VM removal failed"));
            }
            assert!(runtime.machine_manager.storage_hold_path().is_file());
        }
    }

    #[tokio::test]
    async fn initialization_write_failures_publish_protection() {
        for path in ["hold", "status.json"] {
            let directory = tempfile::tempdir().unwrap();
            let runtime = Arc::new(
                Runtime::new(crate::config::Config {
                    data_dir: directory.path().to_owned(),
                    ..Default::default()
                })
                .unwrap(),
            );
            fs::create_dir_all(directory.path().join("storage-recovery").join(path)).unwrap();
            assert!(runtime.recover_storage(Action::CheckOnly).await.is_err());
            let progress = runtime.subscribe_storage_recovery().0.unwrap();
            assert_eq!(progress.phase, Phase::Failed);
            assert!(progress.storage_protected);
            assert!(runtime.storage_writes_protected());
            assert!(!runtime.storage_recovery_active());
        }
    }

    #[tokio::test]
    async fn detached_failure_is_replayed_after_the_request_receiver_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            Runtime::new(crate::config::Config {
                data_dir: directory.path().to_owned(),
                ..Default::default()
            })
            .unwrap(),
        );
        let (started, updates) = runtime.recover_storage(Action::CheckOnly).await.unwrap();
        drop(updates);
        let (initial, mut replay) = runtime.subscribe_storage_recovery();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut current = initial.unwrap();
            while !state::terminal(&current) {
                current = replay.recv().await.unwrap();
            }
            current
        })
        .await
        .unwrap();
        assert_eq!(finished.phase, Phase::Failed);
        assert_eq!(finished.operation_id, started.operation_id);
        assert!(finished.message.contains("System VM storage configuration"));
        assert!(runtime.machine_manager.storage_hold_path().is_file());
    }
}
