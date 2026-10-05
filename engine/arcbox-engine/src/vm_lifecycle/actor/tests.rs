use super::*;
use crate::machine::MachineConfig;
use crate::vm::VmManager;

fn actor(data_dir: &std::path::Path) -> (LifecycleActor, Machine) {
    let bus = EventBus::new();
    let machines = Arc::new(MachineManager::new(
        Arc::new(VmManager::new(data_dir.join("snapshots"))),
        data_dir.to_path_buf(),
        Default::default(),
        bus.clone(),
    ));
    let manager = super::super::VmLifecycleManager::new(
        machines,
        bus,
        data_dir.to_path_buf(),
        VmLifecycleConfig::default(),
    )
    .unwrap();
    let seed = manager.seed.lock().unwrap().take().unwrap();
    let mut actor =
        LifecycleActor::new(manager.shared, seed.commands, manager.cmd_tx, seed.state_tx);
    let machine = VmLifecycle
        .uninitialized_state_machine()
        .init_with_context(&mut actor.effects);
    (actor, machine)
}

async fn completion(actor: &mut LifecycleActor) -> Completion {
    tokio::time::timeout(Duration::from_secs(5), actor.events_rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn overlapping_stops_wait_for_successful_removal() {
    let dir = tempfile::tempdir().unwrap();
    let (mut actor, mut machine) = actor(dir.path());
    actor
        .shared
        .machine_manager
        .create(MachineConfig::default())
        .await
        .unwrap();
    let mut events = actor.shared.event_bus.subscribe();
    let (reply, mut first) = oneshot::channel();
    actor.on_command(&mut machine, Command::ForceStop { reply });
    let epoch = actor.epoch;
    let (reply, mut second) = oneshot::channel();
    actor.on_command(&mut machine, Command::ForceStop { reply });
    let (reply, mut shutdown) = oneshot::channel();
    actor.on_command(&mut machine, Command::Shutdown { reply });

    assert_eq!(actor.epoch, epoch);
    assert_eq!(actor.public(), VmLifecycleState::Stopping);
    assert_eq!(actor.shared.restart_generation.load(Ordering::Acquire), 0);
    assert!(events.try_recv().is_err());
    for reply in [&mut first, &mut second, &mut shutdown] {
        assert!(matches!(
            reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    let event = completion(&mut actor).await;
    assert!(
        first.try_recv().is_err(),
        "only the actor may acknowledge removal"
    );
    actor.on_internal(&mut machine, event);
    assert_eq!(actor.public(), VmLifecycleState::NotExist);
    assert!(actor.shared.machine_manager.get("default").is_none());
    assert_eq!(actor.shared.restart_generation.load(Ordering::Acquire), 1);
    for reply in [first, second, shutdown] {
        reply.await.unwrap().unwrap();
    }
    assert!(matches!(
        events.try_recv(),
        Ok(Event::MachineStopped { .. })
    ));
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn removal_failure_fails_stop_and_ready_waiters_without_stopped_event() {
    let dir = tempfile::tempdir().unwrap();
    let (mut actor, mut machine) = actor(dir.path());
    actor
        .shared
        .machine_manager
        .register_mock_machine("default", 3)
        .unwrap();
    let vm_id = actor.shared.machine_manager.get("default").unwrap().vm_id;
    let mut events = actor.shared.event_bus.subscribe();
    let (reply, stopped) = oneshot::channel();
    actor.on_command(&mut machine, Command::ForceStop { reply });
    let (reply, shutdown) = oneshot::channel();
    actor.on_command(&mut machine, Command::Shutdown { reply });
    let (reply, ready) = oneshot::channel();
    actor.on_command(
        &mut machine,
        Command::EnsureReady {
            timeout: Duration::from_secs(1),
            reply,
        },
    );
    let event = completion(&mut actor).await;
    assert!(
        matches!(&event.outcome, InternalEvent::RemoveFailed(reason) if reason.contains(&vm_id.to_string()))
    );
    actor.on_internal(&mut machine, event);

    assert_eq!(actor.public(), VmLifecycleState::Failed);
    assert!(actor.shared.machine_manager.get("default").is_some());
    assert!(actor.inflight.is_none());
    assert!(actor.pending_timeout.is_none());
    let reason = stopped.await.unwrap().unwrap_err().to_string();
    assert_eq!(shutdown.await.unwrap().unwrap_err().to_string(), reason);
    assert_eq!(ready.await.unwrap().unwrap_err().to_string(), reason);
    assert_eq!(actor.shared.restart_generation.load(Ordering::Acquire), 0);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn force_stop_of_an_absent_machine_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (mut actor, mut machine) = actor(dir.path());
    assert!(matches!(
        actor.shared.machine_manager.remove("default", true),
        Err(EngineError::Common(arcbox_error::CommonError::NotFound(_)))
    ));
    for _ in 0..2 {
        let (reply, stopped) = oneshot::channel();
        actor.on_command(&mut machine, Command::ForceStop { reply });
        let event = completion(&mut actor).await;
        actor.on_internal(&mut machine, event);
        stopped.await.unwrap().unwrap();
        assert_eq!(actor.public(), VmLifecycleState::NotExist);
    }
}

#[tokio::test]
async fn readiness_waits_for_removal_and_stale_completion_cannot_finish_next_boot() {
    let dir = tempfile::tempdir().unwrap();
    let (mut actor, mut machine) = actor(dir.path());
    actor
        .shared
        .machine_manager
        .create(MachineConfig::default())
        .await
        .unwrap();
    let (reply, stopped) = oneshot::channel();
    actor.on_command(&mut machine, Command::ForceStop { reply });
    let removal_epoch = actor.epoch;
    let (reply, mut ready) = oneshot::channel();
    actor.on_command(
        &mut machine,
        Command::EnsureReady {
            timeout: Duration::from_secs(1),
            reply,
        },
    );
    assert_eq!(
        actor.epoch, removal_epoch,
        "readiness must not spawn before removal"
    );
    assert_eq!(actor.public(), VmLifecycleState::Stopping);
    let event = completion(&mut actor).await;
    actor.on_internal(&mut machine, event);
    assert_eq!(actor.public(), VmLifecycleState::Creating);
    assert!(actor.epoch > removal_epoch);
    actor.on_internal(
        &mut machine,
        Completion {
            epoch: removal_epoch,
            outcome: InternalEvent::Removed,
        },
    );
    assert_eq!(actor.public(), VmLifecycleState::Creating);
    assert!(
        actor.inflight.is_some(),
        "stale completion must retain the boot handle"
    );
    assert!(matches!(
        ready.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    actor.abort_inflight();
    stopped.await.unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn reservation_after_force_stop_admission_blocks_removal() {
    let dir = tempfile::tempdir().unwrap();
    let (mut actor, mut machine) = actor(dir.path());
    let machines = Arc::clone(&actor.shared.machine_manager);
    machines.create(MachineConfig::default()).await.unwrap();
    let mut events = actor.shared.event_bus.subscribe();
    let (reply, stopped) = oneshot::channel();
    actor.on_command(&mut machine, Command::ForceStop { reply });
    // The current-thread executor cannot start removal before this test yields.
    let _reservation = machines.reserve_storage().unwrap();
    let event = completion(&mut actor).await;
    actor.on_internal(&mut machine, event);
    let error = stopped.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("held for recovery"));
    assert_eq!(actor.public(), VmLifecycleState::Failed);
    assert!(machines.get("default").is_some());
    assert!(events.try_recv().is_err());
}
