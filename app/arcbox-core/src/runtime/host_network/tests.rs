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
        .await;
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
