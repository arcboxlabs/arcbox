//! Minimal mock guest dockerd for proxy integration tests.
//!
//! A hyper HTTP/1.1 server on a Unix socket that handles:
//! - Normal requests: returns status 200 with the request body echoed back
//! - Upgrade requests (`Upgrade` header present): returns 101 and echoes
//!   data bidirectionally on the upgraded connection
//!
//! The server runs until the returned [`CancellationToken`] is cancelled.

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

/// Start a mock guest dockerd listening on a Unix socket.
///
/// Returns the socket path and a cancellation token to stop the server.
pub async fn start(dir: &Path) -> (PathBuf, CancellationToken) {
    let socket_path = dir.join("mock-guest.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind mock guest socket");
    let cancel = CancellationToken::new();
    let token = cancel.clone();

    tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                result = listener.accept() => match result {
                    Ok((s, _)) => s,
                    Err(_) => break,
                },
                () = token.cancelled() => break,
            };

            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = http1::Builder::new()
                    .serve_connection(io, service_fn(handle))
                    .with_upgrades()
                    .await;
            });
        }
    });

    (socket_path, cancel)
}

async fn handle(
    mut req: Request<Incoming>,
) -> std::result::Result<Response<http_body_util::Full<bytes::Bytes>>, hyper::Error> {
    let wants_upgrade = req.headers().contains_key(hyper::header::UPGRADE);

    if wants_upgrade {
        // Return 101 Switching Protocols and echo on the upgraded stream.
        let upgrade_proto = req
            .headers()
            .get(hyper::header::UPGRADE)
            .cloned()
            .unwrap_or_else(|| hyper::header::HeaderValue::from_static("tcp"));

        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    let mut io = TokioIo::new(upgraded);
                    // Echo: copy everything read back as write.
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
