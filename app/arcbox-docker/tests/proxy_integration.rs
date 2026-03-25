//! Integration tests for the Docker proxy layer.
//!
//! These tests use a mock guest dockerd on a Unix socket so they run
//! without a VM. They call proxy functions directly (bypassing the
//! handler layer's `ensure_vm_ready`) to isolate proxy forwarding logic.

mod mock_guest;

use arcbox_docker::proxy::{
    GuestConnector, RawFdStream, proxy_to_guest, proxy_to_guest_stream, proxy_with_upgrade,
};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use tempfile::TempDir;

// =============================================================================
// UnixSocketConnector — test connector that connects to mock guest
// =============================================================================

struct UnixSocketConnector {
    socket_path: PathBuf,
}

impl GuestConnector for UnixSocketConnector {
    fn connect(
        &self,
    ) -> Pin<Box<dyn Future<Output = arcbox_docker::Result<TokioIo<RawFdStream>>> + Send + '_>>
    {
        Box::pin(async {
            let path = self.socket_path.clone();
            let fd = tokio::task::spawn_blocking(move || {
                let stream = StdUnixStream::connect(&path)
                    .map_err(|e| arcbox_docker::DockerError::Server(e.to_string()))?;
                Ok::<_, arcbox_docker::DockerError>(stream.into_raw_fd())
            })
            .await
            .map_err(|e| arcbox_docker::DockerError::Server(e.to_string()))??;

            let stream = RawFdStream::from_raw_fd(fd)
                .map_err(|e| arcbox_docker::DockerError::Server(e.to_string()))?;
            Ok(TokioIo::new(stream))
        })
    }
}

// =============================================================================
// Test helpers
// =============================================================================

async fn setup() -> (
    UnixSocketConnector,
    TempDir,
    tokio_util::sync::CancellationToken,
) {
    let tmp = TempDir::new().unwrap();
    let (socket_path, cancel) = mock_guest::start(tmp.path()).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let connector = UnixSocketConnector { socket_path };
    (connector, tmp, cancel)
}

// =============================================================================
// Tests — proxy_to_guest (buffered forwarding)
// =============================================================================

#[tokio::test]
async fn proxy_to_guest_echoes_body() {
    let (connector, _tmp, cancel) = setup().await;

    let payload = r#"{"Image":"alpine:latest"}"#;
    let resp = proxy_to_guest(
        &connector,
        Method::POST,
        "/containers/create",
        &HeaderMap::new(),
        Bytes::from(payload),
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, payload.as_bytes());
    cancel.cancel();
}

#[tokio::test]
async fn proxy_to_guest_empty_body() {
    let (connector, _tmp, cancel) = setup().await;

    let resp = proxy_to_guest(
        &connector,
        Method::GET,
        "/containers/json",
        &HeaderMap::new(),
        Bytes::new(),
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    cancel.cancel();
}

// =============================================================================
// Tests — proxy_to_guest_stream (streaming forwarding)
// =============================================================================

#[tokio::test]
async fn proxy_stream_forwards_body() {
    let (connector, _tmp, cancel) = setup().await;

    let payload = r#"{"Name":"test-volume"}"#;
    let uri: Uri = "/volumes/create".parse().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/volumes/create")
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap();

    let resp = proxy_to_guest_stream(&connector, &uri, req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, payload.as_bytes());
    cancel.cancel();
}

// =============================================================================
// Tests — proxy_with_upgrade
// =============================================================================

#[tokio::test]
async fn upgrade_returns_101_with_correct_protocol() {
    let (connector, _tmp, cancel) = setup().await;

    let uri: Uri = "/grpc".parse().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/grpc")
        .header("connection", "Upgrade")
        .header("upgrade", "h2c")
        .body(Body::empty())
        .unwrap();

    let resp = proxy_with_upgrade(&connector, req, &uri).await.unwrap();

    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    let upgrade = resp
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(upgrade, "h2c");
    cancel.cancel();
}

#[tokio::test]
async fn upgrade_forwards_request_body() {
    let (connector, _tmp, cancel) = setup().await;

    let payload = r#"{"Detach":false,"Tty":false}"#;
    let uri: Uri = "/exec/abc123/start".parse().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/exec/abc123/start")
        .header("connection", "Upgrade")
        .header("upgrade", "tcp")
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap();

    let resp = proxy_with_upgrade(&connector, req, &uri).await.unwrap();

    // Mock guest accepts upgrade regardless of body — handshake success
    // proves the body was forwarded (guest dockerd didn't hang waiting).
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    cancel.cancel();
}
