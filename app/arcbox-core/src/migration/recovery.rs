//! Mutual exclusion between migration execution and storage recovery.

use super::*;

pub struct RecoveryReservation(Arc<MigrationManager>);

impl Drop for RecoveryReservation {
    fn drop(&mut self) {
        self.0.storage_recovery.store(false, Ordering::Release);
    }
}

impl MigrationManager {
    pub(crate) async fn reserve_recovery(self: &Arc<Self>) -> Result<RecoveryReservation> {
        let _start = self.run_start.lock().await;
        if self.storage_recovery.load(Ordering::Acquire)
            || self.runs.read().await.values().any(MigrationRun::is_active)
        {
            return Err(CoreError::invalid_state(
                "migration or storage recovery is running",
            ));
        }
        self.storage_recovery.store(true, Ordering::Release);
        Ok(RecoveryReservation(Arc::clone(self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_and_migration_exclude_each_other_in_both_orders() {
        let manager = Arc::new(MigrationManager::new(PathBuf::from("/unused/docker.sock")));
        let recovery = manager.reserve_recovery().await.unwrap();
        let error = manager
            .run_migration(RunMigrationRequest::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("storage recovery is running"));
        assert!(manager.reserve_recovery().await.is_err());
        drop(recovery);
        manager.runs.write().await.insert(
            "migration-1".into(),
            MigrationRun::new(MigrationRunOptions {
                allow_replacements: false,
                skip_start: false,
            }),
        );
        assert!(manager.reserve_recovery().await.is_err());
        manager.runs.write().await.clear();
        assert!(manager.reserve_recovery().await.is_ok());
    }
}
