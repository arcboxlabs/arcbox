use super::*;
use std::time::Duration;

fn runtime() -> (tempfile::TempDir, Arc<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        Runtime::new(crate::config::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap(),
    );
    (directory, runtime)
}

async fn fail_initial_persistence(runtime: &Arc<Runtime>) -> StorageRecoveryProgress {
    fs::write(&runtime.storage_recovery.directory, b"not a directory").unwrap();
    assert!(runtime.recover_storage(Action::CheckOnly).await.is_err());
    fs::remove_file(&runtime.storage_recovery.directory).unwrap();
    assert!(
        !runtime
            .machine_manager
            .storage_hold_path()
            .try_exists()
            .unwrap()
    );
    let progress = runtime.subscribe_storage_recovery().0.unwrap();
    assert_eq!(progress.phase, Phase::Failed);
    assert!(runtime.storage_writes_protected());
    assert!(!runtime.storage_recovery_active());
    progress
}

#[tokio::test]
async fn failed_initial_persistence_blocks_engine_boot_and_allows_recovery_retry() {
    let (_directory, runtime) = runtime();
    let failed = fail_initial_persistence(&runtime).await;
    for machine in [DEFAULT_MACHINE_NAME, "rosetta"] {
        assert!(
            runtime
                .machine_manager
                .ensure_storage_available(machine)
                .is_err()
        );
    }
    assert!(
        runtime
            .machine_manager
            .ensure_storage_available("dev")
            .is_ok()
    );
    let boot = tokio::time::timeout(Duration::from_secs(5), runtime.vm_lifecycle.ensure_ready())
        .await
        .unwrap()
        .unwrap_err();
    assert!(boot.to_string().contains("storage is held for recovery"));

    let (started, _) = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.recover_storage(Action::CheckOnly),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(started.operation_id, failed.operation_id);
    tokio::time::timeout(Duration::from_secs(5), async {
        runtime
            .storage_recovery
            .owner
            .tasks
            .lock()
            .await
            .join()
            .await;
    })
    .await
    .unwrap();
    let finished = runtime.subscribe_storage_recovery().0.unwrap();
    assert_eq!(finished.operation_id, started.operation_id);
    assert_eq!(finished.phase, Phase::Failed);
    assert!(finished.message.contains("System VM storage configuration"));
    assert_eq!(
        fs::read_to_string(runtime.machine_manager.storage_hold_path()).unwrap(),
        started.operation_id
    );
    assert!(runtime.machine_manager.storage_is_held().unwrap());
}

#[tokio::test]
async fn protected_shutdown_reuses_reservation_until_runtime_drop() {
    let (_directory, runtime) = runtime();
    fail_initial_persistence(&runtime).await;
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), runtime.close_storage_recovery())
                .await
                .unwrap()
                .unwrap()
        );
        assert!(runtime.machine_manager.storage_is_held().unwrap());
        assert!(
            !runtime
                .machine_manager
                .storage_hold_path()
                .try_exists()
                .unwrap()
        );
    }
    let manager = Arc::clone(&runtime.machine_manager);
    drop(runtime);
    assert!(!manager.storage_is_held().unwrap());
}

#[test]
fn failed_completion_retains_reservation_until_a_successful_commit() {
    let (_directory, runtime) = runtime();
    runtime
        .persist_storage_protection(StorageRecoveryProgress {
            operation_id: "completion-failure".into(),
            storage_protected: true,
            ..Default::default()
        })
        .unwrap();
    let owner = &runtime.storage_recovery.owner;
    assert!(
        owner
            .complete(|| {
                fs::remove_file(runtime.machine_manager.storage_hold_path())?;
                Err(CoreError::Machine("journal write failed".into()))
            })
            .is_err()
    );
    assert!(runtime.machine_manager.storage_is_held().unwrap());
    drop(owner.reserve(&runtime.machine_manager).unwrap());
    assert!(runtime.machine_manager.storage_is_held().unwrap());
    owner.complete(|| Ok(())).unwrap();
    assert!(!runtime.machine_manager.storage_is_held().unwrap());
}

#[tokio::test]
async fn worker_panic_after_hold_removal_retains_reservation_through_shutdown() {
    let (_directory, runtime) = runtime();
    runtime
        .persist_storage_protection(StorageRecoveryProgress {
            operation_id: "completion-panic".into(),
            storage_protected: true,
            ..Default::default()
        })
        .unwrap();
    let worker = Arc::clone(&runtime);
    runtime.storage_recovery.owner.tasks.lock().await.worker = Some(tokio::spawn(async move {
        worker.storage_recovery.owner.complete(|| {
            fs::remove_file(worker.machine_manager.storage_hold_path())?;
            panic!("completion failed after removing the hold");
        })
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        runtime
            .storage_recovery
            .owner
            .tasks
            .lock()
            .await
            .join()
            .await;
    })
    .await
    .unwrap();
    assert!(
        !runtime
            .machine_manager
            .storage_hold_path()
            .try_exists()
            .unwrap()
    );
    assert!(runtime.machine_manager.storage_is_held().unwrap());
    let error = tokio::time::timeout(Duration::from_secs(5), runtime.close_storage_recovery())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("completion failed after removing the hold")
    );
    assert!(runtime.machine_manager.storage_is_held().unwrap());
    assert!(runtime.machine_manager.storage_hold_path().is_file());
}
