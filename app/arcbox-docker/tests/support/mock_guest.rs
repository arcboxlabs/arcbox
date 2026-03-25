//! Minimal mock guest dockerd for proxy integration tests.
//!
//! A hyper HTTP/1.1 server on a Unix socket that handles:
//! - Normal requests: returns status 200 with the request body echoed back
//! - Upgrade requests (`Upgrade` header present): returns 101 and acts
//!   as a simple echo server, sending back any data received on the
//!   upgraded connection
//!
//! The server runs until the returned [`CancellationToken`] is cancelled.

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Handle returned by [`start`] to interact with the mock server.
pub struct MockGuest {
    pub socket_path: PathBuf,
    pub cancel: CancellationToken,
    /// Body received on the most recent upgrade request (if any).
    last_upgrade_body: Arc<Mutex<Option<Bytes>>>,
}

impl MockGuest {
    /// Returns the body received on the most recent upgrade request.
    pub async fn last_upgrade_body(&self) -> Option<Bytes> {
        self.last_upgrade_body.lock().await.clone()
    }
}

/// Start a mock guest dockerd listening on a Unix socket.
pub async fn start(dir: &Path) -> MockGuest {
    let socket_path = dir.join("mock-guest.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind mock guest socket");
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let last_upgrade_body: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
    let body_slot = Arc::clone(&last_upgrade_body);

    tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                result = listener.accept() => match result {
                    Ok((s, _)) => s,
                    Err(_) => break,
                },
                () = token.cancelled() => break,
            };

            let slot = Arc::clone(&body_slot);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req| {
                    let slot = Arc::clone(&slot);
                    handle(req, slot)
                });
                let _ = http1::Builder::new()
                    .serve_connection(io, svc)
                    .with_upgrades()
                    .await;
            });
        }
    });

    MockGuest {
        socket_path,
        cancel,
        last_upgrade_body,
    }
}

async fn handle(
    mut req: Request<Incoming>,
    upgrade_body_slot: Arc<Mutex<Option<Bytes>>>,
) -> std::result::Result<Response<http_body_util::Full<Bytes>>, hyper::Error> {
    let wants_upgrade = req.headers().contains_key(hyper::header::UPGRADE);

    if wants_upgrade {
        let upgrade_proto = req
            .headers()
            .get(hyper::header::UPGRADE)
            .cloned()
            .unwrap_or_else(|| hyper::header::HeaderValue::from_static("tcp"));

        // Collect the request body before the upgrade so tests can verify
        // that the proxy forwarded it.
        let body_bytes = http_body_util::BodyExt::collect(req.body_mut())
            .await
            .ok()
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        *upgrade_body_slot.lock().await = Some(body_bytes);

        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    let mut io = TokioIo::new(upgraded);
                    let (mut rd, mut wr) = tokio::io::split(&mut io);
                    let _ = tokio::io::copy(&mut rd, &mut wr).await;
                }
                Err(e) => tracing::debug!("mock guest upgrade failed: {e}"),
            }
        });

        let resp = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(hyper::header::UPGRADE, upgrade_proto)
            .body(http_body_util::Full::default())
            .unwrap();
        return Ok(resp);
    }

    // Normal request: echo the body back with 200.
    let body_bytes = http_body_util::BodyExt::collect(req.into_body())
        .await?
        .to_bytes();

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(http_body_util::Full::new(body_bytes))
        .unwrap();
    Ok(resp)
}
