//! Catch-all proxy handler for unmatched Docker API routes.

use super::{forward, upgrade, upload};
use crate::api::AppState;
use crate::error::{DockerError, Result};
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{Response, header};

/// Catch-all handler that proxies unmatched requests to guest dockerd.
///
/// Ensures forward compatibility with newer Docker API versions — any endpoint
/// we don't explicitly handle gets forwarded transparently.
///
/// # Errors
///
/// Returns an error if VM readiness fails or guest proxying fails.
pub async fn proxy_fallback(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    req: axum::http::Request<Body>,
) -> Result<Response<Body>> {
    tracing::debug!("proxy_fallback: method={} uri={}", req.method(), uri);
    state
        .runtime
        .ensure_vm_ready()
        .await
        .map_err(|e| DockerError::Server(format!("failed to ensure VM is ready: {e}")))?;

    // Detect upgrade requests (attach, exec, BuildKit gRPC/session).
    let wants_upgrade = req.headers().get(header::UPGRADE).is_some()
        || req
            .headers()
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));

    if wants_upgrade {
        return upgrade::proxy_with_upgrade(state.connector.as_ref(), req, &uri).await;
    }

    if upload::is_streaming_upload_request(req.method(), &uri) {
        return upload::proxy_streaming_upload(state.connector.as_ref(), &uri, req).await;
    }

    forward::proxy_to_guest_stream(state.connector.as_ref(), &uri, req).await
}
