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
    let hosts = runtime.network_manager.local_hosts_table();
    let hosts = hosts.read().unwrap();
    assert_eq!(
        hosts.get("old.arcbox.local"),
        Some(&"172.17.0.2".parse().unwrap())
    );
    assert!(!hosts.contains_key("new.arcbox.local"));
}
