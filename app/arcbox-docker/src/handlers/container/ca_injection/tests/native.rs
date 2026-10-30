//! Native archive injection check against an externally owned disposable fixture.

use bytes::Bytes;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use super::*;
use crate::proxy::HalfCloseStream;

async fn bundle(container: &Container<'_>) -> Vec<u8> {
    let response = container
        .request(
            Method::GET,
            &format!("archive?path={}", BUNDLES[0]),
            Bytes::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    untar(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .1
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ARCBOX_CA_TEST_NATIVE_SOCKET and NATIVE_CONTAINER from the isolated overlay-archive fixture"]
async fn native_archive_injection_survives_immediate_start_and_rotation() {
    let socket = PathBuf::from(std::env::var("ARCBOX_CA_TEST_NATIVE_SOCKET").unwrap());
    let id = std::env::var("ARCBOX_CA_TEST_NATIVE_CONTAINER").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let bridge_socket = directory.path().join("bridge.sock");
    let listener = UnixListener::bind(&bridge_socket).unwrap();
    let stop = CancellationToken::new();
    let bridge_stop = stop.clone();
    let bridge = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = bridge_stop.cancelled() => {
                    connections.shutdown().await;
                    break;
                }
                accepted = listener.accept() => {
                    let (incoming, _) = accepted.unwrap();
                    let mut upstream = UnixStream::connect(&socket).await.unwrap();
                    connections.spawn(async move {
                        tokio::io::copy_bidirectional(
                            &mut HalfCloseStream::new(incoming), &mut upstream,
                        ).await.unwrap();
                    });
                }
                Some(result) = connections.join_next() => result.unwrap(),
            }
        }
    }));
    let state = guest::state(bridge_socket, directory.path());
    let container = Container {
        state: &state,
        id: &id,
    };
    let original = bundle(&container).await;
    assert_eq!(original, b"original roots\n");
    for _ in 0..20 {
        let pem = root();
        assert_eq!(
            reconcile_with_pem(&state, &id, &pem).await.unwrap(),
            Vec::<String>::new()
        );
        let injected = bundle(&container).await;
        assert!(injected.starts_with(&original));
        assert_eq!(
            injected
                .windows(pem.trim_ascii().len())
                .filter(|part| *part == pem.trim_ascii())
                .count(),
            1
        );
        assert_eq!(
            reconcile_with_pem(&state, &id, &pem).await.unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(bundle(&container).await, injected);
        let started = container
            .request(Method::POST, "start", Bytes::new())
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::NO_CONTENT);
        let waited = container
            .request(Method::POST, "wait?condition=not-running", Bytes::new())
            .await
            .unwrap();
        assert_eq!(waited.status(), StatusCode::OK);
        let waited: serde_json::Value =
            serde_json::from_slice(&to_bytes(waited.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(waited["StatusCode"], 0);
        assert!(waited["Error"].is_null());
        assert_eq!(bundle(&container).await, injected);
    }
    stop.cancel();
    bridge.await.unwrap();
    drop(state);
}
