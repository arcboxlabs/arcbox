use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::Context as _;

use super::*;

struct FakeObserver {
    generation: AtomicU64,
    calls: AtomicUsize,
    connection_fails: AtomicBool,
    updates: watch::Receiver<StorageHealth>,
    health: std::sync::Mutex<Option<StorageHealth>>,
}

impl Observer for FakeObserver {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn set_health(&self, generation: u64, health: Option<StorageHealth>) {
        assert_eq!(generation, self.generation());
        *self.health.lock().unwrap() = health;
    }

    async fn observe(&self, publish: impl Fn(StorageHealth) + Send + Sync) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.connection_fails.load(Ordering::SeqCst) {
            anyhow::bail!("test connection failed");
        }
        let mut updates = self.updates.clone();
        loop {
            publish(updates.borrow_and_update().clone());
            updates.changed().await.context("test stream closed")?;
        }
    }
}

async fn settled() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stopped_vm_is_not_probed_and_idle_keeps_the_existing_watch() {
    let (updates, snapshots) = watch::channel(StorageHealth {
        observed_at_unix_ms: 1,
        ..Default::default()
    });
    let observer = Arc::new(FakeObserver {
        generation: AtomicU64::new(0),
        calls: AtomicUsize::new(0),
        connection_fails: AtomicBool::new(false),
        updates: snapshots,
        health: std::sync::Mutex::new(None),
    });
    let (vm_tx, vm) = watch::channel(VmLifecycleState::Stopped);
    let setup = Arc::new(SetupState::new());
    let shutdown = CancellationToken::new();
    let task = {
        let observer = observer.clone();
        let setup = setup.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { observe_loop(observer.as_ref(), vm, &setup, &shutdown).await })
    };
    settled().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
    vm_tx.send_replace(VmLifecycleState::Running);
    settled().await;
    assert_eq!(setup.current().storage_health.observed_at_unix_ms, 1);
    vm_tx.send_replace(VmLifecycleState::Idle);
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    observer.generation.fetch_add(1, Ordering::SeqCst);
    vm_tx.send_replace(VmLifecycleState::Stopped);
    updates.send_replace(StorageHealth {
        observed_at_unix_ms: 2,
        ..Default::default()
    });
    settled().await;
    assert!(!setup.current().storage_health.is_set());
    assert!(observer.health.lock().unwrap().is_none());
    vm_tx.send_replace(VmLifecycleState::Running);
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 2);
    assert_eq!(setup.current().storage_health.observed_at_unix_ms, 2);
    shutdown.cancel();
    task.await.unwrap();
    assert!(!setup.current().storage_health.is_set());
}

#[tokio::test(start_paused = true)]
async fn failed_connection_retries_without_a_vm_restart() {
    let (_updates, snapshots) = watch::channel(StorageHealth {
        observed_at_unix_ms: 42,
        ..Default::default()
    });
    let observer = Arc::new(FakeObserver {
        generation: AtomicU64::new(0),
        calls: AtomicUsize::new(0),
        connection_fails: AtomicBool::new(true),
        updates: snapshots,
        health: std::sync::Mutex::new(None),
    });
    let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
    let setup = Arc::new(SetupState::new());
    let shutdown = CancellationToken::new();
    let task = {
        let observer = observer.clone();
        let setup = setup.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { observe_loop(observer.as_ref(), vm, &setup, &shutdown).await })
    };
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    assert!(!setup.current().storage_health.is_set());
    observer.connection_fails.store(false, Ordering::SeqCst);
    tokio::time::advance(
        RECONNECT_DELAY
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    )
    .await;
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 2);
    assert_eq!(setup.current().storage_health.observed_at_unix_ms, 42);
    vm_tx.send_replace(VmLifecycleState::Idle);
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 2);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn lost_watch_clears_fault_and_stopped_vm_cancels_reconnect() {
    use arcbox_connect::v1::{StorageVolumeHealth, storage_volume_health::State};

    let (updates, snapshots) = watch::channel(StorageHealth {
        volumes: vec![StorageVolumeHealth {
            state: State::ReadOnly.into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let observer = Arc::new(FakeObserver {
        generation: AtomicU64::new(0),
        calls: AtomicUsize::new(0),
        connection_fails: AtomicBool::new(false),
        updates: snapshots,
        health: std::sync::Mutex::new(None),
    });
    let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
    let setup = Arc::new(SetupState::new());
    let shutdown = CancellationToken::new();
    let task = {
        let observer = observer.clone();
        let setup = setup.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { observe_loop(observer.as_ref(), vm, &setup, &shutdown).await })
    };
    settled().await;
    assert_eq!(
        setup.current().storage_health.volumes[0].state,
        State::ReadOnly
    );
    drop(updates);
    settled().await;
    assert!(!setup.current().storage_health.is_set());
    assert!(observer.health.lock().unwrap().is_none());
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    vm_tx.send_replace(VmLifecycleState::Stopped);
    settled().await;
    tokio::time::advance(RECONNECT_DELAY * 2).await;
    settled().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    shutdown.cancel();
    task.await.unwrap();
}
