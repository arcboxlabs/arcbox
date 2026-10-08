//! Covers cancellation before connection and draining an already-started connection.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[tokio::test]
async fn cancelled_or_invalidated_poll_starts_no_connection() {
    for vm_changed in [false, true] {
        let mut source = Recording::default();
        let shutdown = CancellationToken::new();
        let (vm_tx, mut vm) = watch::channel(VmLifecycleState::Running);
        if vm_changed {
            vm_tx.send_replace(VmLifecycleState::Stopped);
        } else {
            shutdown.cancel();
        }
        let response = reconcile(
            &mut source,
            &0,
            ReconcileHttpsAccessRequest {
                enabled: true,
                ..Default::default()
            },
            &shutdown,
            &mut vm,
        )
        .await;
        assert!(response.is_err());
        assert_eq!(source.calls(), []);
    }
}

#[derive(Clone, Default)]
struct Connecting {
    source: Recording,
    started: Arc<Notify>,
    release: Arc<Notify>,
    cancelled: Arc<Mutex<Option<CancellationToken>>>,
    drained: Arc<AtomicBool>,
}

impl AccessSource for Connecting {
    type Vm = u64;

    fn incarnation(&self) -> Result<Self::Vm> {
        self.source.incarnation()
    }

    fn with_vm<T>(&self, expected: &Self::Vm, publish: impl FnOnce() -> T) -> Result<T> {
        self.source.with_vm(expected, publish)
    }

    fn host_addresses(&self) -> Result<Vec<String>> {
        self.source.host_addresses()
    }

    fn resolve(&self, hostname: &str) -> Option<IpAddr> {
        self.source.resolve(hostname)
    }

    async fn reconcile(
        &mut self,
        expected: &Self::Vm,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        if !request.enabled {
            assert!(self.drained.load(Ordering::SeqCst));
            return self.source.reconcile(expected, request, cancelled).await;
        }
        *self.cancelled.lock().unwrap() = Some(cancelled.clone());
        self.started.notify_one();
        self.release.notified().await;
        self.drained.store(true, Ordering::SeqCst);
        ensure!(
            cancelled.is_cancelled(),
            "connection must finish after cancellation"
        );
        bail!("connection drained")
    }
}

#[tokio::test]
async fn lifecycle_cancellation_drains_the_connection_before_disabling() {
    for vm_changed in [false, true] {
        tokio::time::timeout(Duration::from_secs(2), async {
            let setup = Arc::new(SetupState::new());
            status(&setup, TrustState::Untrusted);
            let _client = watch(setup.clone()).await;
            let source = Connecting::default();
            let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(observe(
                source.clone(),
                setup.https_access.clone(),
                vm,
                shutdown.clone(),
            ));
            source.started.notified().await;
            if vm_changed {
                vm_tx.send_replace(VmLifecycleState::Stopped);
            } else {
                shutdown.cancel();
            }
            let cancelled = source.cancelled.lock().unwrap().clone().unwrap();
            cancelled.cancelled().await;
            assert!(!task.is_finished());
            assert!(!source.drained.load(Ordering::SeqCst));
            assert_eq!(source.source.calls(), []);
            source.release.notify_one();
            shutdown.cancel();
            task.await.unwrap();
            assert!(source.drained.load(Ordering::SeqCst));
            let calls = source.source.calls();
            assert_eq!(calls.len(), 1);
            assert!(!calls[0].enabled);
        })
        .await
        .unwrap();
    }
}
