//! Exercises machine task ownership without a native VM or Keychain.

use std::{collections::HashSet, sync::Mutex};

use arcbox_engine::event::{Event, EventBus};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::*;

#[derive(Debug)]
struct Delivery {
    name: String,
    changed: watch::Receiver<()>,
    cancelled: oneshot::Receiver<()>,
    finish: oneshot::Sender<Result<()>>,
}

struct Harness {
    names: Arc<Mutex<HashSet<String>>>,
    events: EventBus,
    shutdown: CancellationToken,
    started: mpsc::UnboundedReceiver<Delivery>,
    task: JoinHandle<Result<()>>,
}

impl Harness {
    fn new(names: &[&str]) -> Self {
        let names = Arc::new(Mutex::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<HashSet<_>>(),
        ));
        let source = Arc::clone(&names);
        let events = EventBus::new();
        let shutdown = CancellationToken::new();
        let (started, deliveries) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_machines(
            events.subscribe(),
            shutdown.clone(),
            IDENTITY_INTERVAL,
            move || source.lock().unwrap().clone(),
            move |name, changed, shutdown| {
                let started = started.clone();
                async move {
                    let (cancel, cancelled) = oneshot::channel();
                    let (finish, mut finished) = oneshot::channel();
                    started
                        .send(Delivery {
                            name,
                            changed,
                            cancelled,
                            finish,
                        })
                        .unwrap();
                    tokio::select! {
                        result = &mut finished => result?,
                        () = shutdown.cancelled() => {
                            cancel.send(()).unwrap();
                            finished.await?
                        }
                    }
                }
            },
        ));
        Self {
            names,
            events,
            shutdown,
            started: deliveries,
            task,
        }
    }

    async fn next(&mut self) -> Delivery {
        tokio::time::timeout(Duration::from_secs(2), self.started.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn set(&self, names: &[&str], event: Event) {
        *self.names.lock().unwrap() = names.iter().map(|name| (*name).to_owned()).collect();
        self.events.publish(event);
    }
}

#[tokio::test]
async fn stopping_one_machine_preserves_its_peer_and_joins_before_same_name_restart() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut harness = Harness::new(&["a", "b"]);
        let first = harness.next().await;
        let second = harness.next().await;
        let (mut a, mut b) = if first.name == "a" {
            (first, second)
        } else {
            (second, first)
        };
        b.changed.borrow_and_update();
        harness.set(&["b"], Event::MachineStopping { name: "a".into() });
        (&mut a.cancelled).await.unwrap();
        b.changed.changed().await.unwrap();
        assert_eq!(
            b.cancelled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );

        harness.set(
            &["a", "b"],
            Event::MachineStopCancelled { name: "a".into() },
        );
        b.changed.changed().await.unwrap();
        assert!(
            harness.started.try_recv().is_err(),
            "replacement started before the old worker joined"
        );
        a.finish.send(Ok(())).unwrap();
        let mut replacement = harness.next().await;
        assert_eq!(replacement.name, "a");

        harness.shutdown.cancel();
        (&mut b.cancelled).await.unwrap();
        (&mut replacement.cancelled).await.unwrap();
        assert!(!harness.task.is_finished());
        b.finish.send(Ok(())).unwrap();
        assert!(
            !harness.task.is_finished(),
            "shutdown abandoned a retained worker"
        );
        replacement.finish.send(Ok(())).unwrap();
        harness.task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_queued_stopping_event_cancels_delivery_even_if_the_record_already_resumed() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut harness = Harness::new(&["a"]);
        let mut old = harness.next().await;
        harness
            .events
            .publish(Event::MachineStopping { name: "a".into() });
        harness
            .events
            .publish(Event::MachineStopCancelled { name: "a".into() });
        (&mut old.cancelled).await.unwrap();
        old.finish.send(Ok(())).unwrap();
        let mut current = harness.next().await;
        assert_eq!(current.name, "a");
        harness.shutdown.cancel();
        (&mut current.cancelled).await.unwrap();
        current.finish.send(Ok(())).unwrap();
        harness.task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_lagged_event_source_rescans_running_and_removed_machine_names() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut harness = Harness::new(&["a"]);
        let mut removed = harness.next().await;
        *harness.names.lock().unwrap() = HashSet::from(["b".into()]);
        for _ in 0..300 {
            harness.events.publish(Event::MachineCreated {
                name: "noise".into(),
            });
        }
        (&mut removed.cancelled).await.unwrap();
        let mut added = harness.next().await;
        assert_eq!(added.name, "b");
        removed.finish.send(Ok(())).unwrap();
        harness.shutdown.cancel();
        (&mut added.cancelled).await.unwrap();
        added.finish.send(Ok(())).unwrap();
        harness.task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_worker_failure_cancels_and_joins_other_machine_deliveries() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut harness = Harness::new(&["a", "b"]);
        let failed = harness.next().await;
        let mut pending = harness.next().await;
        failed
            .finish
            .send(Err(anyhow::anyhow!("injected worker failure")))
            .unwrap();
        (&mut pending.cancelled).await.unwrap();
        assert!(harness.shutdown.is_cancelled());
        assert!(!harness.task.is_finished());
        pending.finish.send(Ok(())).unwrap();
        let error = harness.task.await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("injected worker failure"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_before_supervision_never_starts_machine_work() {
    let bus = EventBus::new();
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    supervise_machines(
        bus.subscribe(),
        shutdown,
        IDENTITY_INTERVAL,
        || HashSet::from(["a".into()]),
        |_, _, _| async { panic!("cancelled service started a machine worker") },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_closed_event_source_joins_pending_machine_work() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut harness = Harness::new(&["a"]);
        let mut delivery = harness.next().await;
        drop(harness.events);
        (&mut delivery.cancelled).await.unwrap();
        assert!(!harness.task.is_finished());
        delivery.finish.send(Ok(())).unwrap();
        let error = harness.task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("event source closed"));
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_running_machine_without_a_started_event_is_discovered_on_rescan() {
    let mut harness = Harness::new(&["existing"]);
    let mut existing = harness.next().await;
    harness.names.lock().unwrap().insert("recovered".into());
    tokio::time::advance(IDENTITY_INTERVAL).await;
    let mut recovered = harness.next().await;
    assert_eq!(recovered.name, "recovered");
    harness.shutdown.cancel();
    (&mut existing.cancelled).await.unwrap();
    (&mut recovered.cancelled).await.unwrap();
    existing.finish.send(Ok(())).unwrap();
    recovered.finish.send(Ok(())).unwrap();
    harness.task.await.unwrap().unwrap();
}
