//! Exercises lifecycle cancellation and failure cleanup without a guest VM.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct Signals {
    started: Notify,
    cancelled: Notify,
    release: Notify,
    disconnected: Notify,
    calls: AtomicUsize,
    disconnects: AtomicUsize,
    drained: AtomicBool,
}

struct BlockedSource {
    signals: Arc<Signals>,
    connecting: bool,
}

impl ServiceSource for BlockedSource {
    async fn list(
        &mut self,
        cancelled: &CancellationToken,
    ) -> arcbox_core::Result<(VmIncarnation, KubernetesLoadBalancersResponse)> {
        self.signals.calls.fetch_add(1, Ordering::SeqCst);
        self.signals.started.notify_one();
        if self.connecting {
            self.signals.release.notified().await;
            assert!(cancelled.is_cancelled());
        } else {
            cancelled.cancelled().await;
            self.signals.cancelled.notify_one();
            self.signals.release.notified().await;
        }
        self.signals.drained.store(true, Ordering::SeqCst);
        Err(arcbox_core::CoreError::invalid_state("cancelled listing"))
    }

    fn disconnect(&mut self) {
        self.signals.disconnects.fetch_add(1, Ordering::SeqCst);
        self.signals.disconnected.notify_one();
    }
}

async fn runtime() -> (Arc<Runtime>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        Runtime::new(arcbox_core::Config {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .with_network_fixture(arcbox_core::runtime::NetworkFixture::default()),
    );
    runtime.vm_lifecycle().set_kubernetes_hold(true).await;
    (runtime, dir)
}

#[tokio::test]
async fn cancelled_poll_starts_no_guest_connection() {
    let (runtime, _dir) = runtime().await;
    let signals = Arc::new(Signals::default());
    signals.release.notify_one();
    let mut source = BlockedSource {
        signals: Arc::clone(&signals),
        connecting: true,
    };
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(poll(&runtime, &mut source, &cancelled).await.is_err());
    assert_eq!(signals.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_changes_cancel_and_drain_before_disconnecting() {
    for trigger in ["hold", "vm", "shutdown", "connecting"] {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (runtime, _dir) = runtime().await;
            let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
            let shutdown = CancellationToken::new();
            let signals = Arc::new(Signals::default());
            let task = tokio::spawn({
                let runtime = Arc::clone(&runtime);
                let shutdown = shutdown.clone();
                let source = BlockedSource {
                    signals: Arc::clone(&signals),
                    connecting: trigger == "connecting",
                };
                async move {
                    reconcile_loop(
                        &runtime,
                        vm,
                        runtime.subscribe_kubernetes_hold(),
                        shutdown,
                        source,
                    )
                    .await;
                }
            });
            signals.started.notified().await;
            match trigger {
                "hold" => runtime.vm_lifecycle().set_kubernetes_hold(false).await,
                "vm" => {
                    vm_tx.send_replace(VmLifecycleState::Stopping);
                }
                _ => shutdown.cancel(),
            }
            if trigger == "connecting" {
                tokio::task::yield_now().await;
            } else {
                signals.cancelled.notified().await;
            }
            assert!(!task.is_finished(), "must drain {trigger}");
            assert!(!signals.drained.load(Ordering::SeqCst));
            assert_eq!(signals.calls.load(Ordering::SeqCst), 1);
            assert_eq!(signals.disconnects.load(Ordering::SeqCst), 0);
            signals.release.notify_one();
            signals.disconnected.notified().await;
            assert!(signals.drained.load(Ordering::SeqCst));
            shutdown.cancel();
            task.await.unwrap();
            assert_eq!(signals.calls.load(Ordering::SeqCst), 1);
        })
        .await
        .unwrap();
    }
}

struct FailingSource(Arc<Signals>);

impl ServiceSource for FailingSource {
    async fn list(
        &mut self,
        _cancelled: &CancellationToken,
    ) -> arcbox_core::Result<(VmIncarnation, KubernetesLoadBalancersResponse)> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.started.notify_one();
        Err(arcbox_core::CoreError::invalid_state("guest unavailable"))
    }

    fn disconnect(&mut self) {
        self.0.disconnects.fetch_add(1, Ordering::SeqCst);
        self.0.disconnected.notify_one();
    }
}

#[tokio::test]
async fn listing_failure_discards_the_connection_before_retrying() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (runtime, _dir) = runtime().await;
        let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let signals = Arc::new(Signals::default());
        let task = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            let shutdown = shutdown.clone();
            let source = FailingSource(Arc::clone(&signals));
            async move {
                reconcile_loop(
                    &runtime,
                    vm,
                    runtime.subscribe_kubernetes_hold(),
                    shutdown,
                    source,
                )
                .await;
            }
        });
        signals.started.notified().await;
        signals.disconnected.notified().await;
        assert_eq!(signals.calls.load(Ordering::SeqCst), 1);
        assert_eq!(signals.disconnects.load(Ordering::SeqCst), 1);
        shutdown.cancel();
        task.await.unwrap();
    })
    .await
    .unwrap();
}
