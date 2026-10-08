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
