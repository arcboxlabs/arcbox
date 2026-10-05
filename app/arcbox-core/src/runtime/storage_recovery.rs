//! Reconciles interrupted recovery journals before System VM admission.

mod state;

use crate::Runtime;
use arcbox_connect::v1::StorageRecoveryProgress;
pub(super) use state::StorageRecovery;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;

impl Runtime {
    /// Reports storage protection restored from the recovery journal and durable hold.
    #[must_use]
    pub fn storage_writes_protected(&self) -> bool {
        self.storage_recovery.protected.load(Ordering::Acquire)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_connect::v1::storage_recovery_progress::Phase;

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
}
