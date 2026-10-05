//! Durable progress and reconnect replay for a daemon-owned recovery operation.

use crate::error::Result;
use arcbox_connect::v1::{StorageRecoveryProgress, storage_recovery_progress::Phase};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use tokio::sync::{broadcast, watch};

#[derive(Serialize, Deserialize)]
struct Record {
    operation_id: String,
    phase: i32,
    message: String,
    recovery_directory: String,
    storage_protected: bool,
}

impl From<&StorageRecoveryProgress> for Record {
    fn from(value: &StorageRecoveryProgress) -> Self {
        Self {
            operation_id: value.operation_id.clone(),
            phase: value.phase.to_i32(),
            message: value.message.clone(),
            recovery_directory: value.recovery_directory.clone(),
            storage_protected: value.storage_protected,
        }
    }
}

pub(in crate::runtime) struct StorageRecovery {
    pub(super) protected: AtomicBool,
    pub(in crate::runtime) directory: PathBuf,
    latest: watch::Sender<Option<StorageRecoveryProgress>>,
    updates: broadcast::Sender<StorageRecoveryProgress>,
}

impl StorageRecovery {
    pub(in crate::runtime) fn load(data_dir: &Path) -> Result<Self> {
        let directory = data_dir.join("storage-recovery");
        let path = directory.join("status.json");
        let hold = directory.join("hold");
        let mut reconciled = false;
        let initial = if path.try_exists()? {
            let record: Record = serde_json::from_slice(&fs::read(&path)?)?;
            let mut progress = StorageRecoveryProgress {
                operation_id: record.operation_id,
                phase: record.phase.into(),
                message: record.message,
                recovery_directory: record.recovery_directory,
                storage_protected: record.storage_protected,
                ..Default::default()
            };
            if hold.try_exists()? {
                let operation_id = fs::read_to_string(&hold)?;
                if operation_id != progress.operation_id {
                    progress = StorageRecoveryProgress {
                        operation_id,
                        phase: Phase::Stopping.into(),
                        storage_protected: true,
                        ..Default::default()
                    };
                }
            }
            if !terminal(&progress) {
                arcbox_atomic_file::write(
                    &directory.join("hold"),
                    progress.operation_id.as_bytes(),
                )?;
                progress.phase = Phase::Failed.into();
                progress.storage_protected = true;
                progress.message = "Storage recovery was interrupted; the outcome is unverified. Run the check again.".into();
                reconciled = true;
            }
            progress.storage_protected |= hold.try_exists()?;
            if progress.storage_protected && !hold.try_exists()? {
                arcbox_atomic_file::write(&hold, progress.operation_id.as_bytes())?;
            }
            Some(progress)
        } else if hold.try_exists()? {
            let operation_id = fs::read_to_string(&hold)?;
            let progress = StorageRecoveryProgress {
                phase: Phase::Failed.into(),
                message: "Storage recovery was interrupted before its journal was written. The disks remain protected and unverified.".into(),
                operation_id,
                storage_protected: true,
                ..Default::default()
            };
            reconciled = true;
            Some(progress)
        } else {
            None
        };
        let state = Self {
            protected: AtomicBool::new(directory.join("hold").try_exists()?),
            directory,
            latest: watch::channel(None).0,
            updates: broadcast::channel(32).0,
        };
        if let Some(progress) = initial {
            if reconciled {
                state.publish(progress)?;
            } else {
                state.latest.send_replace(Some(progress));
            }
        }
        Ok(state)
    }

    pub(super) fn subscribe(
        &self,
    ) -> (
        Option<StorageRecoveryProgress>,
        broadcast::Receiver<StorageRecoveryProgress>,
    ) {
        let snapshot = self.latest.borrow();
        let updates = self.updates.subscribe();
        (snapshot.clone(), updates)
    }

    pub(super) fn publish(&self, progress: StorageRecoveryProgress) -> Result<()> {
        arcbox_atomic_file::write(
            &self.directory.join("status.json"),
            &serde_json::to_vec_pretty(&Record::from(&progress))?,
        )?;
        self.latest.send_modify(|latest| {
            *latest = Some(progress.clone());
            let _ = self.updates.send(progress);
        });
        Ok(())
    }
}

pub(super) fn terminal(progress: &StorageRecoveryProgress) -> bool {
    matches!(
        progress.phase.as_known(),
        Some(Phase::Complete | Phase::Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_without_journal_is_visible_and_protected_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let recovery = dir.path().join("storage-recovery");
        fs::create_dir(&recovery).unwrap();
        fs::write(recovery.join("hold"), b"interrupted-operation").unwrap();
        let state = StorageRecovery::load(dir.path()).unwrap();
        let progress = state.subscribe().0.unwrap();
        assert_eq!(progress.phase, Phase::Failed);
        assert_eq!(progress.operation_id, "interrupted-operation");
        assert!(progress.storage_protected);
        assert!(state.protected.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn newer_hold_replaces_an_older_completed_outcome_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = StorageRecovery::load(dir.path()).unwrap();
        fs::create_dir(&state.directory).unwrap();
        state
            .publish(StorageRecoveryProgress {
                operation_id: "completed-operation".into(),
                phase: Phase::Complete.into(),
                message: "Verification passed".into(),
                ..Default::default()
            })
            .unwrap();
        fs::write(state.directory.join("hold"), b"new-operation").unwrap();
        let restored = StorageRecovery::load(dir.path()).unwrap();
        let progress = restored.subscribe().0.unwrap();
        assert_eq!(progress.operation_id, "new-operation");
        assert_eq!(progress.phase, Phase::Failed);
        assert!(progress.storage_protected);
        assert!(progress.message.contains("interrupted"));
        assert_eq!(progress.recovery_directory, "");
    }

    #[test]
    fn interrupted_verification_restores_boot_hold_before_replaying_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = StorageRecovery::load(dir.path()).unwrap();
        fs::create_dir(&state.directory).unwrap();
        state
            .publish(StorageRecoveryProgress {
                operation_id: "operation-1".into(),
                phase: Phase::Verifying.into(),
                recovery_directory: "/preserved/pair".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(!state.directory.join("hold").exists());
        drop(state);
        let restored = StorageRecovery::load(dir.path()).unwrap();
        let (progress, _) = restored.subscribe();
        let progress = progress.unwrap();
        assert_eq!(progress.phase, Phase::Failed);
        assert_eq!(progress.operation_id, "operation-1");
        assert_eq!(progress.recovery_directory, "/preserved/pair");
        assert_eq!(
            fs::read(restored.directory.join("hold")).unwrap(),
            b"operation-1"
        );
    }

    #[test]
    fn completed_recovery_replays_without_creating_boot_hold() {
        let dir = tempfile::tempdir().unwrap();
        let state = StorageRecovery::load(dir.path()).unwrap();
        fs::create_dir(&state.directory).unwrap();
        state
            .publish(StorageRecoveryProgress {
                operation_id: "operation-2".into(),
                phase: Phase::Complete.into(),
                ..Default::default()
            })
            .unwrap();
        let restored = StorageRecovery::load(dir.path()).unwrap();
        assert_eq!(restored.subscribe().0.unwrap().phase, Phase::Complete);
        assert!(!restored.directory.join("hold").exists());
    }

    #[test]
    fn progress_replay_does_not_drop_consecutive_phases() {
        let dir = tempfile::tempdir().unwrap();
        let state = StorageRecovery::load(dir.path()).unwrap();
        fs::create_dir(&state.directory).unwrap();
        let (_, mut updates) = state.subscribe();
        for phase in [Phase::Stopping, Phase::Preserving, Phase::Checking] {
            state
                .publish(StorageRecoveryProgress {
                    phase: phase.into(),
                    ..Default::default()
                })
                .unwrap();
        }
        for phase in [Phase::Stopping, Phase::Preserving, Phase::Checking] {
            assert_eq!(updates.try_recv().unwrap().phase, phase);
        }
        assert!(updates.try_recv().is_err());
    }
}
