//! Requests by a name and a full ID must retain the same start guard.

use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, time::timeout};

use super::*;

#[tokio::test]
async fn a_slow_stop_does_not_block_another_canonical_containers_guest_or_host_work() {
    let guest = guest::Guest::new(metadata()).await;
    let release = Arc::new(Notify::new());
    guest.data.lock().unwrap().block_operation = Some(Arc::clone(&release));
    let state = guest.state.clone();
    let stop = tokio::spawn(async move { operation(state, "test", "stop").await });
    wait_for_request(&guest, "/containers/canonical-test/stop").await;

    let response = timeout(
        Duration::from_secs(2),
        operation(guest.state.clone(), "other", "start"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        guest
            .state
            .runtime
            .resolve_registered_container("other")
            .await
            .as_deref(),
        Some("canonical-other")
    );
    assert!(
        !stop.is_finished(),
        "the first guest response is still held"
    );
    release.notify_one();
    assert_eq!(
        stop.await.unwrap().unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

async fn wait_for_request(guest: &guest::Guest, path: &str) {
    let changed = Arc::clone(&guest.data.lock().unwrap().changed);
    timeout(Duration::from_secs(2), async {
        loop {
            let notified = changed.notified();
            if guest
                .data
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|p| p == path)
            {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn alias_start_blocks_canonical_restart_until_the_guest_responds() {
    let guest = guest::Guest::new(metadata()).await;
    let release = Arc::new(Notify::new());
    guest.data.lock().unwrap().block_operation = Some(Arc::clone(&release));
    let state = guest.state.clone();
    let start = tokio::spawn(async move { operation(state, "test", "start").await });
    wait_for_request(&guest, "/containers/canonical-test/start").await;

    let state = guest.state.clone();
    let mut restart =
        tokio::spawn(async move { operation(state, "canonical-test", "restart").await });
    wait_for_request(&guest, "/containers/canonical-test/json").await;
    assert!(
        timeout(Duration::from_millis(25), &mut restart)
            .await
            .is_err()
    );
    assert_eq!(
        guest.data.lock().unwrap().operations,
        ["/containers/canonical-test/start"]
    );

    release.notify_one();
    assert_eq!(
        start.await.unwrap().unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        restart.await.unwrap().unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        guest.data.lock().unwrap().operations,
        [
            "/containers/canonical-test/start",
            "/containers/canonical-test/restart"
        ]
    );
}

#[tokio::test]
async fn inspection_body_failure_invalidates_cached_readiness() {
    let guest = guest::Guest::new(metadata()).await;
    guest.data.lock().unwrap().oversized_inspect = true;
    let error = Lifecycle::acquire(&guest.state, "test")
        .await
        .err()
        .unwrap();
    assert!(error.is_guest_transport());
    guest.data.lock().unwrap().oversized_inspect = false;
    let _start = Lifecycle::acquire(&guest.state, "test").await.unwrap();
    assert_eq!(
        guest
            .data
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|path| *path == "/_ping")
            .count(),
        2
    );
}
