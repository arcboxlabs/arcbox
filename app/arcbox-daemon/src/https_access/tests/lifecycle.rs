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

    fn with_claim<T>(
        &self,
        expected: &Self::Vm,
        hostname: &str,
        publish: impl FnOnce(IpAddr) -> T,
    ) -> Result<Option<T>> {
        self.source.with_claim(expected, hostname, publish)
    }

    fn host_addresses(&self, expected: &Self::Vm) -> Result<Vec<String>> {
        self.source.host_addresses(expected)
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
                VmLifecycleState::is_ready,
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

#[tokio::test]
async fn stopped_or_stalled_system_does_not_block_machine_observation() {
    use crate::background_task::BackgroundTask;
    use std::{pin::pin, task::Poll};

    tokio::time::timeout(Duration::from_secs(3), async {
        for state in [VmLifecycleState::Stopped, VmLifecycleState::Running] {
            let ready = state.is_ready();
            let setup = Arc::new(SetupState::new());
            status(&setup, TrustState::Untrusted);
            let mut client = watch(setup.clone()).await;
            assert_eq!(client.next().await.unwrap().unwrap().request_id, "");
            let system = Connecting::default();
            let machine = Recording::default();
            *machine.response.lock().unwrap() = ReconcileHttpsAccessResponse {
                hostname: "web.arcbox.local".into(),
                destination_ip: "10.0.2.3".into(),
                sequence: 1,
                ..Default::default()
            };
            let (_vm_tx, vm) = watch::channel(state);
            let (_machine_tx, changed) = watch::channel(());
            let shutdown = CancellationToken::new();
            let observation = observe(
                machine.clone(),
                setup.https_access.clone(),
                changed,
                |()| true,
                shutdown.clone(),
            );
            let service = BackgroundTask::default();
            service
                .start(super::super::workers::run(
                    observe(
                        system.clone(),
                        setup.https_access.clone(),
                        vm,
                        VmLifecycleState::is_ready,
                        shutdown.clone(),
                    ),
                    async move {
                        observation.await;
                        Ok(())
                    },
                    shutdown.clone(),
                ))
                .await;
            if ready {
                system.started.notified().await;
            }
            assert_eq!(
                client.next().await.unwrap().unwrap().hostname,
                "web.arcbox.local"
            );
            assert!(!system.drained.load(Ordering::SeqCst));
            assert_eq!(system.source.calls(), []);
            shutdown.cancel();
            let mut joining = pin!(service.join());
            if ready {
                let cancelled = system.cancelled.lock().unwrap().clone().unwrap();
                cancelled.cancelled().await;
                assert!(matches!(futures::poll!(&mut joining), Poll::Pending));
                system.release.notify_one();
            }
            joining.await.unwrap();
            assert!(!machine.calls().last().unwrap().enabled);
            assert_eq!(system.drained.load(Ordering::SeqCst), ready);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn machine_supervisor_joins_connection_cleanup_before_disabling() {
    use crate::{background_task::BackgroundTask, guest_workers::supervise_machines};
    use std::{collections::HashSet, pin::pin, task::Poll};

    tokio::time::timeout(Duration::from_secs(3), async {
        let setup = Arc::new(SetupState::new());
        status(&setup, TrustState::Untrusted);
        let _client = watch(setup.clone()).await;
        let source = Connecting::default();
        let worker = source.clone();
        let names = Arc::new(Mutex::new(HashSet::from(["machine".into()])));
        let scan = Arc::clone(&names);
        let (events, receiver) = tokio::sync::broadcast::channel(4);
        let shutdown = CancellationToken::new();
        let service = BackgroundTask::default();
        service
            .start(supervise_machines(
                receiver,
                shutdown.clone(),
                POLL_INTERVAL,
                move || scan.lock().unwrap().clone(),
                move |_, changed, cancelled| {
                    let observation = observe(
                        worker.clone(),
                        setup.https_access.clone(),
                        changed,
                        |()| true,
                        cancelled,
                    );
                    async move {
                        observation.await;
                        Ok(())
                    }
                },
            ))
            .await;
        source.started.notified().await;
        names.lock().unwrap().clear();
        events
            .send(arcbox_core::event::Event::MachineStopping {
                name: "machine".into(),
            })
            .unwrap();
        let cancelled = source.cancelled.lock().unwrap().clone().unwrap();
        cancelled.cancelled().await;
        shutdown.cancel();
        let mut joining = pin!(service.join());
        assert!(matches!(futures::poll!(&mut joining), Poll::Pending));
        assert_eq!(source.source.calls(), []);
        source.release.notify_one();
        joining.await.unwrap();
        assert!(source.drained.load(Ordering::SeqCst));
        let calls = source.source.calls();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].enabled);
    })
    .await
    .unwrap();
}
