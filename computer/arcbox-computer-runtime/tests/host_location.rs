//! The manager at the host location, over drivers shaped like the host's.
//!
//! The host drivers are built elsewhere (W1's per-sandbox HV process, W5's
//! VZ checkpoint); what this manager depends on is only what they claim.
//! `FakeDriver` narrowed to those claims is that dependency, exactly:
//!
//! - VZ runs its VMs in the daemon — no `Prepare`, nothing to adopt, no
//!   staging area — and checkpoints only while the session is unlocked.
//! - HV spawns one VMM process per sandbox — `Prepare` and `Adopt` — and
//!   reads host paths, so it has no staging area either.
//!
//! Neither runs under the jailer. Firecracker's tests in
//! `manager_over_fakes.rs` are the nested location and are unchanged.

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arcbox_computer_runtime::testkit::agent::Reply;
use arcbox_computer_runtime::{
    CheckpointGate, ComputerError, ComputerMountSpec, ComputerSpec, ComputerState, IdleAction,
    Placement, RestoreComputerSpec, RunLocation, pause_reason,
};
use arcbox_vm_driver::testkit::FakeDriver;
use arcbox_vm_driver::{DriverCapabilities, VmDriver as _, VmId};
use support::{Fixture, Setup, action, drain_actions};

const BUILD: &str = "25E5207k";

fn vz_driver() -> FakeDriver {
    FakeDriver::builder()
        .capabilities(DriverCapabilities {
            vsock: true,
            checkpoint: true,
            balloon: true,
            console: true,
            agent_log: true,
            port_forward: true,
            debug: true,
            ..DriverCapabilities::default()
        })
        .build()
}

fn hv_driver() -> FakeDriver {
    FakeDriver::builder()
        .capabilities(DriverCapabilities {
            vsock: true,
            checkpoint: true,
            adopt: true,
            prepare: true,
            port_forward: true,
            balloon: true,
            debug: true,
            ..DriverCapabilities::default()
        })
        .build()
}

/// The screen lock, as the VZ composer's gate reports it.
#[derive(Default)]
struct SessionLock(AtomicBool);

impl CheckpointGate for SessionLock {
    fn checkpoint_available(&self) -> Result<(), String> {
        if self.0.load(Ordering::SeqCst) {
            Err("the session is locked; VZ cannot save a checkpoint until it is unlocked".into())
        } else {
            Ok(())
        }
    }
}

/// Pause, resume, data plane, and a checkpoint restored into a clone: the
/// whole lifecycle over one host driver.
async fn full_cycle(fixture: &Fixture) {
    fixture.agent().on(&["/bin/hello"], Reply::stdout(b"hi"));
    let id = fixture.ready("host").await;
    assert_eq!(fixture.run(&id, &["/bin/hello"]).await, b"hi");

    fixture.manager.pause_computer(&id).await.unwrap();
    fixture.await_state(&id, ComputerState::Paused).await;
    fixture.settle_network_cleanups().await;
    fixture
        .manager
        .resume_computer(&id, pause_reason::RESUME)
        .await
        .unwrap();
    fixture.await_state(&id, ComputerState::Ready).await;
    assert_eq!(fixture.run(&id, &["/bin/hello"]).await, b"hi");

    let checkpoint = fixture
        .manager
        .checkpoint_computer(&id, "user".into(), HashMap::new())
        .await
        .unwrap();
    let (clone, _) = fixture
        .manager
        .restore_computer(RestoreComputerSpec {
            id: Some("clone".into()),
            snapshot_id: checkpoint.snapshot_id,
            network_override: true,
            ..RestoreComputerSpec::default()
        })
        .await
        .unwrap();
    fixture.await_state(&clone, ComputerState::Ready).await;
    assert!(
        fixture
            .driver()
            .restored_vms()
            .contains(&VmId::new(&clone).unwrap()),
        "the clone came from the checkpoint"
    );
    assert_eq!(fixture.run(&clone, &["/bin/hello"]).await, b"hi");

    for id in [&id, &clone] {
        fixture.manager.remove_computer(id, false).await.unwrap();
        fixture.await_gone(id).await;
    }
}

/// VZ: no `Prepare`, no `Staging`, no jailer. The VMM starts at boot, the
/// VM is handed host paths, and a copied rootfs still leaves the VM's own
/// area and comes back across a pause.
#[tokio::test]
async fn a_vz_shaped_driver_runs_the_whole_lifecycle() {
    let fixture = Setup::host(vz_driver(), Placement::host(BUILD))
        .build()
        .await;
    let capabilities = fixture.manager.capabilities();
    assert_eq!(capabilities.location, RunLocation::Host);
    assert!(capabilities.pause_resume);
    assert_eq!(capabilities.pause_unavailable, None);
    assert!(!capabilities.warm_prespawn, "nothing to spawn ahead");
    assert!(capabilities.port_publishing);
    full_cycle(&fixture).await;
}

/// HV: `Prepare` and `Adopt`, no `Staging`, no jailer.
#[tokio::test]
async fn an_hv_shaped_driver_runs_the_whole_lifecycle() {
    let fixture = Setup::host(hv_driver(), Placement::host(BUILD))
        .build()
        .await;
    let capabilities = fixture.manager.capabilities();
    assert!(capabilities.pause_resume);
    assert!(
        capabilities.warm_prespawn,
        "a VMM process can be spawned ahead"
    );
    full_cycle(&fixture).await;
}

/// A host VM that outlives the daemon is adopted by the next one, the way
/// an HV sandbox's VMM process outlives it.
#[tokio::test]
async fn an_hv_sandbox_is_adopted_across_a_daemon_restart() {
    let mut fixture = Setup::host(hv_driver(), Placement::host(BUILD))
        .build()
        .await;
    fixture.agent().on(&["/bin/hello"], Reply::stdout(b"hi"));
    let id = fixture.ready("survivor").await;

    fixture.manager.detach_all().await.unwrap();
    fixture = fixture.restart().await;
    fixture.await_state(&id, ComputerState::Ready).await;
    assert_eq!(fixture.run(&id, &["/bin/hello"]).await, b"hi");

    // Its disk is reachable through the adopted handle alone.
    fixture.manager.pause_computer(&id).await.unwrap();
    fixture.await_state(&id, ComputerState::Paused).await;
}

/// While the session is locked a VZ pause is refused before anything is
/// frozen, and `GetCapabilities` says so; once unlocked it runs.
#[tokio::test]
async fn a_vz_pause_is_refused_while_the_session_is_locked() {
    let lock = Arc::new(SessionLock::default());
    let fixture = Setup::host(
        vz_driver(),
        Placement::host(BUILD).with_checkpoint_gate(lock.clone()),
    )
    .build()
    .await;
    let id = fixture.ready("locked").await;
    let mut events = fixture.manager.subscribe_events();

    lock.0.store(true, Ordering::SeqCst);
    let unavailable = fixture.manager.capabilities().pause_unavailable;
    assert!(
        unavailable.as_deref().is_some_and(|r| r.contains("locked")),
        "{unavailable:?}"
    );
    match fixture.manager.pause_computer(&id).await {
        Err(ComputerError::FailedPrecondition(reason)) => assert!(reason.contains("locked")),
        other => panic!("expected FAILED_PRECONDITION, got {other:?}"),
    }
    // A user checkpoint is a VZ save too.
    assert!(matches!(
        fixture
            .manager
            .checkpoint_computer(&id, "user".into(), HashMap::new())
            .await,
        Err(ComputerError::FailedPrecondition(_))
    ));
    assert_eq!(
        fixture.manager.inspect_computer(&id).unwrap().state,
        ComputerState::Ready
    );
    assert!(
        !drain_actions(&mut events, &id).contains(&action::PAUSING.to_owned()),
        "a refused pause never starts"
    );

    lock.0.store(false, Ordering::SeqCst);
    assert_eq!(fixture.manager.capabilities().pause_unavailable, None);
    fixture.manager.pause_computer(&id).await.unwrap();
    fixture.await_state(&id, ComputerState::Paused).await;
}

/// The idle detector is held to the same rule: nothing schedules a VZ save
/// while the Mac is locked (ADR 0010, decision 6). The computer stays up.
#[tokio::test(start_paused = true)]
async fn an_idle_vz_pause_does_not_save_while_the_session_is_locked() {
    let lock = Arc::new(SessionLock(AtomicBool::new(true)));
    let fixture = Setup::host(
        vz_driver(),
        Placement::host(BUILD).with_checkpoint_gate(lock),
    )
    .build()
    .await;
    let id = fixture
        .booted(ComputerSpec {
            id: Some("idler".into()),
            idle_timeout_seconds: 2,
            on_idle: IdleAction::Pause,
            ..ComputerSpec::default()
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    fixture.await_state(&id, ComputerState::Ready).await;
    assert!(
        fixture.manager.list_checkpoints(None).unwrap().is_empty()
            && fixture.snapshot_catalog().list(&id).unwrap().is_empty(),
        "no checkpoint was written"
    );
}

/// D3: a sandbox with bind mounts cannot pause.
#[tokio::test]
async fn a_sandbox_with_bind_mounts_cannot_pause() {
    let fixture = Setup::host(hv_driver(), Placement::host(BUILD))
        .build()
        .await;
    let id = fixture
        .booted(ComputerSpec {
            id: Some("mounted".into()),
            mounts: vec![ComputerMountSpec {
                source: "/Users/me/src".into(),
                target: "/src".into(),
                readonly: false,
            }],
            ..ComputerSpec::default()
        })
        .await;
    match fixture.manager.pause_computer(&id).await {
        Err(ComputerError::FailedPrecondition(reason)) => {
            assert!(reason.contains("bind mounts"), "{reason}");
        }
        other => panic!("expected FAILED_PRECONDITION, got {other:?}"),
    }
    assert_eq!(
        fixture.manager.inspect_computer(&id).unwrap().state,
        ComputerState::Ready
    );
}

/// The nested location keeps its contract: the driver it is given is
/// Firecracker's shape, and without the jailer nothing pauses.
#[tokio::test]
async fn the_nested_location_reports_what_it_always_did() {
    let jailed = Fixture::jailed().await;
    let capabilities = jailed.manager.capabilities();
    assert_eq!(capabilities.location, RunLocation::Nested);
    assert!(capabilities.pause_resume && capabilities.warm_prespawn);
    assert!(capabilities.port_publishing && !capabilities.bind_mounts);

    let direct = Fixture::direct().await;
    assert!(!direct.manager.capabilities().pause_resume);
    assert_eq!(
        direct.driver().capabilities(),
        FakeDriver::new().capabilities()
    );
}
