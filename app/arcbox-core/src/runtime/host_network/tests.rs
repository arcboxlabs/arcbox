use super::*;
use crate::{Config, VmLifecycleConfig};
use std::sync::Arc;

async fn runtime() -> (Runtime, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_vm_lifecycle_config(
        Config {
            data_dir: dir.path().into(),
            ..Config::default()
        },
        VmLifecycleConfig {
            skip_vm_check: true,
            ..VmLifecycleConfig::default()
        },
    )
    .unwrap();
    runtime.ensure_system_vm_ready().await.unwrap();
    (runtime, dir)
}

#[tokio::test]
async fn missing_replaced_and_stopped_physical_vms_reject_admission() {
    let (runtime, _dir) = runtime().await;
    assert!(runtime.container_network_incarnation().is_err());
    let fixture = NetworkFixture::default();
    let runtime = runtime.with_network_fixture(fixture.clone());
    let old = runtime.container_network_incarnation().unwrap();
    fixture.restart();
    assert!(runtime.begin_container_mutation(old).await.is_err());
    assert_eq!(runtime.host_network.activity.lock().unwrap().active, 0);
    fixture.stop();
    assert!(runtime.container_network_incarnation().is_err());
}

#[tokio::test]
async fn cancellation_invalidates_activity_without_waiting_for_host_commit() {
    let (runtime, _dir) = runtime().await;
    let runtime = Arc::new(runtime.with_network_fixture(NetworkFixture::default()));
    let before = runtime.host_network.activity.lock().unwrap().generation;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move {
            let vm = runtime.container_network_incarnation().unwrap();
            let _lease = runtime.begin_container_mutation(vm).await.unwrap();
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        }
    });
    ready.await.unwrap();
    let commit = runtime.host_network.commit.lock().await;
    let admitted = runtime.host_network.activity.lock().unwrap().generation;
    assert!(admitted > before);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let activity = runtime.host_network.activity.lock().unwrap();
    assert_eq!(activity.active, 0);
    assert!(activity.generation > admitted);
    drop(activity);
    drop(commit);
}

#[tokio::test]
async fn vm_replacement_during_alias_wait_rejects_final_publication() {
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    let (runtime, _dir) = runtime().await;
    let fixture = NetworkFixture::default();
    let runtime = runtime.with_network_fixture(fixture.clone());
    let metadata = ContainerMetadata::from_inspect(&serde_json::json!({
        "Id": "canonical", "Name": "/old",
        "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "172.17.0.2"}}}
    }))
    .unwrap();
    let lease = runtime
        .begin_container_mutation(runtime.container_network_incarnation().unwrap())
        .await
        .unwrap();
    lease
        .commit(ContainerChange::Publish(&metadata, &[]))
        .await
        .unwrap();
    let mut renamed = metadata.clone();
    renamed.name = "new".into();
    let aliases = runtime.container_aliases.write().await;
    let mut publish = std::pin::pin!(lease.commit(ContainerChange::Publish(&renamed, &[])));
    assert!(
        publish
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    fixture.restart();
    drop(aliases);
    assert!(publish.await.is_err());
    assert_eq!(
        runtime.resolve_registered_container("old").await.as_deref(),
        Some("canonical")
    );
    assert_eq!(runtime.resolve_registered_container("new").await, None);
    let registry = runtime.dns_registry.read().unwrap();
    assert!(matches!(
        registry.resolve(&"old.arcbox.local".parse().unwrap()),
        arcbox_dns::registry::Lookup::Matched { address, .. }
            if address == "172.17.0.2".parse::<std::net::IpAddr>().unwrap()
    ));
    assert!(matches!(
        registry.resolve(&"new.arcbox.local".parse().unwrap()),
        arcbox_dns::registry::Lookup::Unmatched
    ));
}

#[tokio::test]
async fn snapshots_skip_active_mutations_and_reject_changed_generations_or_vms() {
    let (runtime, _dir) = runtime().await;
    let fixture = NetworkFixture::default();
    let runtime = runtime.with_network_fixture(fixture.clone());
    let before = runtime.capture_container_snapshot().await.unwrap().unwrap();
    let during = runtime.capture_container_snapshot().await.unwrap().unwrap();
    let vm = runtime.container_network_incarnation().unwrap();
    let mutation = runtime.begin_container_mutation(vm).await.unwrap();
    assert!(
        runtime
            .capture_container_snapshot()
            .await
            .unwrap()
            .is_none()
    );
    let mut metadata = ContainerMetadata::from_inspect(&serde_json::json!({
        "Id": "canonical", "Name": "/current",
        "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "172.17.0.2"}}}
    }))
    .unwrap();
    mutation
        .commit(ContainerChange::Publish(&metadata, &[]))
        .await
        .unwrap();
    assert!(!during.commit(&[]).await.unwrap());
    drop(mutation);
    assert!(!before.commit(&[]).await.unwrap());
    let replaced = runtime.capture_container_snapshot().await.unwrap().unwrap();
    let epoch = runtime.dns_registry.read().unwrap().epoch();
    fixture.restart();
    assert!(replaced.commit(&[]).await.is_err());
    assert_eq!(runtime.dns_registry.read().unwrap().epoch(), epoch);
    assert_eq!(
        runtime
            .resolve_registered_container("current")
            .await
            .as_deref(),
        Some("canonical")
    );

    runtime
        .register_host_dns(&["host".into()], "10.0.2.1".parse().unwrap())
        .await
        .unwrap();
    metadata.name = "refreshed".into();
    let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
    assert!(
        snapshot
            .commit(&[ContainerNetwork {
                metadata,
                bindings: vec![]
            }])
            .await
            .unwrap()
    );
    assert_eq!(runtime.resolve_registered_container("current").await, None);
    assert_eq!(
        runtime
            .resolve_registered_container("refreshed")
            .await
            .as_deref(),
        Some("canonical")
    );
    let registry = runtime.dns_registry.read().unwrap();
    assert!(registry.get(super::super::HOST_DNS_OWNER).is_some());
    assert!(matches!(
        registry.resolve(&"current.arcbox.local".parse().unwrap()),
        arcbox_dns::registry::Lookup::Unmatched
    ));
    assert!(matches!(
        registry.resolve(&"refreshed.arcbox.local".parse().unwrap()),
        arcbox_dns::registry::Lookup::Matched { .. }
    ));
}

#[tokio::test]
async fn shared_writers_invalidate_snapshots_and_reject_closed_admission() {
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    let (runtime, _dir) = runtime().await;
    let runtime = runtime.with_network_fixture(NetworkFixture::default());
    let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
    let commit = runtime.host_network.commit.lock().await;
    let names = ["web".into()];
    let mut register =
        std::pin::pin!(runtime.register_dns("web", &names, "172.17.0.2".parse().unwrap()));
    assert!(
        register
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(runtime.dns_registry.read().unwrap().get("web").is_none());
    drop(commit);
    register.await.unwrap();
    assert!(!snapshot.commit(&[]).await.unwrap());
    assert!(runtime.dns_registry.read().unwrap().get("web").is_some());
    let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
    runtime.stop_port_forwarding_all().await;
    assert!(!snapshot.commit(&[]).await.unwrap());
    runtime.host_network.activity.lock().unwrap().closed = true;
    assert!(
        runtime
            .register_container_alias("late", "late")
            .await
            .is_err()
    );
    assert!(
        runtime
            .register_sandbox_dns("late", "172.17.0.3".parse().unwrap())
            .await
            .is_err()
    );
    assert!(
        runtime
            .register_host_dns(&names, "172.17.0.4".parse().unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        runtime.registered_container_ids().await,
        std::collections::HashSet::from(["web".into()])
    );
    assert!(runtime.sandbox_dns_ids.read().await.is_empty());
    runtime.deregister_dns_by_id("web").await;
    assert!(runtime.registered_container_ids().await.is_empty());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn stale_and_failed_snapshots_preserve_a_healthy_listener() {
    use arcbox_net::darwin::inbound_relay::{InboundCommand, InboundListenerManager};
    let (runtime, _dir) = runtime().await;
    let runtime = runtime.with_network_fixture(NetworkFixture::default());
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    runtime
        .adopt_test_manager(
            runtime.default_machine_name(),
            InboundListenerManager::new(tx),
        )
        .await;
    let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
    let probe = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let metadata =
        ContainerMetadata::from_inspect(&serde_json::json!({"Id":"web", "Name":"/web"})).unwrap();
    let mutation = runtime
        .begin_container_mutation(runtime.container_network_incarnation().unwrap())
        .await
        .unwrap();
    mutation
        .commit(ContainerChange::Publish(
            &metadata,
            &[("127.0.0.1".into(), port, 81, "tcp".into())],
        ))
        .await
        .unwrap();
    drop(mutation);
    assert!(!snapshot.commit(&[]).await.unwrap());
    let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let failing = ContainerNetwork {
        metadata: ContainerMetadata::from_inspect(
            &serde_json::json!({"Id":"blocked", "Name":"/blocked"}),
        )
        .unwrap(),
        bindings: vec![(
            "127.0.0.1".into(),
            occupied.local_addr().unwrap().port(),
            82,
            "tcp".into(),
        )],
    };
    let healthy = ContainerNetwork {
        metadata,
        bindings: vec![("127.0.0.1".into(), port, 81, "tcp".into())],
    };
    let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
    assert!(snapshot.commit(&[failing, healthy]).await.is_err());
    let _stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap(),
        Some(InboundCommand::TcpAccepted {
            container_port: 81,
            ..
        })
    ));
    runtime.stop_port_forwarding_all().await;
}

#[tokio::test]
async fn a_new_vm_mutation_retires_all_previous_containers_before_publication() {
    let (runtime, _dir) = runtime().await;
    let fixture = NetworkFixture::default();
    let runtime = runtime.with_network_fixture(fixture.clone());
    for owner in ["system:host", "machine:other", "sandbox:other"] {
        runtime
            .register_dns(
                owner,
                &[format!("{}.local", owner.replace(':', "-"))],
                "127.0.0.1".parse().unwrap(),
            )
            .await
            .unwrap();
    }
    let metadata = |id: &str| {
        ContainerMetadata::from_inspect(&serde_json::json!({
            "Id": id, "Name": format!("/{id}"),
            "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "172.17.0.2"}}}
        }))
        .unwrap()
    };
    let containers = ["old-one", "old-two"].map(|id| ContainerNetwork {
        metadata: metadata(id),
        bindings: vec![],
    });
    runtime
        .capture_container_snapshot()
        .await
        .unwrap()
        .unwrap()
        .commit(&containers)
        .await
        .unwrap();
    fixture.restart();
    let mutation = runtime
        .begin_container_mutation(runtime.container_network_incarnation().unwrap())
        .await
        .unwrap();
    mutation
        .commit(ContainerChange::Publish(&metadata("new"), &[]))
        .await
        .unwrap();
    drop(mutation);
    assert_eq!(
        runtime.registered_container_ids().await,
        std::collections::HashSet::from(["new".into()])
    );
    assert_eq!(runtime.resolve_registered_container("old-one").await, None);
    assert_eq!(runtime.resolve_registered_container("old-two").await, None);
    assert!(
        !runtime
            .retire_system_vm_container_networking()
            .await
            .unwrap()
    );
    fixture.stop();
    assert!(
        runtime
            .retire_system_vm_container_networking()
            .await
            .unwrap()
    );
    assert!(runtime.registered_container_ids().await.is_empty());
    assert_eq!(runtime.resolve_registered_container("new").await, None);
    let registry = runtime.dns_registry.read().unwrap();
    assert!(registry.get("old-one").is_none());
    assert!(registry.get("old-two").is_none());
    assert!(registry.get("new").is_none());
    for owner in ["system:host", "machine:other", "sandbox:other"] {
        assert!(registry.get(owner).is_some());
    }
}

#[tokio::test]
async fn shutdown_closes_admission_before_cleanup_and_rejects_delayed_publication() {
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    for force in [false, true] {
        let (runtime, _dir) = runtime().await;
        let runtime = runtime.with_network_fixture(NetworkFixture::default());
        runtime
            .register_sandbox_dns("existing", "172.17.0.2".parse().unwrap())
            .await
            .unwrap();
        runtime
            .register_container_alias("existing", "existing")
            .await
            .unwrap();
        let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
        let mutation = runtime
            .begin_container_mutation(runtime.container_network_incarnation().unwrap())
            .await
            .unwrap();
        let aliases = runtime.container_aliases.write().await;
        let mut writer = std::pin::pin!(runtime.register_container_alias("late", "late"));
        assert!(
            writer
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let mut shutdown = std::pin::pin!(async {
            if force {
                runtime.shutdown_force().await
            } else {
                runtime.shutdown().await
            }
        });
        assert!(
            shutdown
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(runtime.host_network.activity.lock().unwrap().closed);
        drop(aliases);
        assert!(
            writer
                .await
                .unwrap_err()
                .to_string()
                .contains("admission is closed")
        );
        shutdown.await.unwrap();
        assert!(
            snapshot
                .commit(&[])
                .await
                .unwrap_err()
                .to_string()
                .contains("admission is closed")
        );
        assert!(
            mutation
                .commit(ContainerChange::Remove("existing"))
                .await
                .unwrap_err()
                .to_string()
                .contains("admission is closed")
        );
        assert!(
            runtime
                .register_sandbox_dns("late", "172.17.0.3".parse().unwrap())
                .await
                .is_err()
        );
        assert!(
            runtime
                .start_port_forwarding_for(runtime.default_machine_name(), "late", &[])
                .await
                .is_err()
        );
        assert!(runtime.container_aliases.read().await.is_empty());
        assert!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .owners()
                .next()
                .is_none()
        );
        assert!(runtime.sandbox_dns_ids.read().await.is_empty());
        assert!(runtime.sandbox_port_keys.read().await.is_empty());
        runtime.deregister_sandbox_dns("existing").await;
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn canceled_retirement_retains_ownership_and_preserves_a_new_shared_listener() {
    use arcbox_net::darwin::inbound_relay::{
        InboundCommand, InboundListenerManager, InboundProtocol,
    };
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    let (runtime, _dir) = runtime().await;
    let fixture = NetworkFixture::default();
    let runtime = runtime.with_network_fixture(fixture.clone());
    let metadata =
        ContainerMetadata::from_inspect(&serde_json::json!({"Id":"old", "Name":"/old"})).unwrap();
    let mutation = runtime
        .begin_container_mutation(runtime.container_network_incarnation().unwrap())
        .await
        .unwrap();
    mutation
        .commit(ContainerChange::Publish(&metadata, &[]))
        .await
        .unwrap();
    drop(mutation);
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut manager = InboundListenerManager::new(tx);
    let port = manager
        .add_rule(std::net::Ipv4Addr::LOCALHOST, 0, 8080, InboundProtocol::Tcp)
        .await
        .unwrap();
    runtime
        .adopt_test_manager(runtime.default_machine_name(), manager)
        .await;
    let aliases = runtime.container_aliases.write().await;
    {
        let mut retire = std::pin::pin!(runtime.retire_system_vm_container_networking());
        assert!(
            retire
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert!(
        runtime
            .host_network
            .commit
            .lock()
            .await
            .incarnation
            .is_some()
    );
    assert!(runtime.dns_registry.read().unwrap().get("old").is_some());
    drop(aliases);
    assert!(
        runtime
            .retire_system_vm_container_networking()
            .await
            .unwrap()
    );
    assert!(runtime.dns_registry.read().unwrap().get("old").is_none());
    let _stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap(),
        Some(InboundCommand::TcpAccepted {
            container_port: 8080,
            ..
        })
    ));
    fixture.stop();
    assert!(
        runtime
            .retire_system_vm_container_networking()
            .await
            .unwrap()
    );
    assert!(
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    );
    assert!(
        runtime
            .host_network
            .commit
            .lock()
            .await
            .incarnation
            .is_none()
    );
    assert!(runtime.inbound_listeners.read().await.is_empty());
}
