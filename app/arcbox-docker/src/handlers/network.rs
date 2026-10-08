//! Network handlers that keep host DNS in sync.
//!
//! `POST /networks/{id}/connect` and `/disconnect` change a running
//! container's IP and reachable aliases, so after proxying them to guest
//! dockerd the container's host DNS entry must be refreshed — otherwise
//! resolving the container by name keeps returning the pre-connect address.
//! All other network operations proxy through the fallback untouched (host
//! routing uses one static container-subnet route, not per-network state).

use super::{container::lifecycle::Lifecycle, proxying::proxy_to_system_vm};
use crate::api::AppState;
use crate::error::{DockerError, Result};
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{Request, Uri};
use axum::response::Response;

/// Connect a container to a network, then refresh its host DNS entry.
///
/// # Errors
///
/// Returns an error if VM readiness fails or proxying to guest dockerd fails.
#[tracing::instrument(
    name = "docker.network.connect",
    skip(state, req),
    fields(uri = %uri, utility_vm = "native"),
    err
)]
pub async fn network_connect(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    req: Request<Body>,
) -> Result<Response> {
    proxy_then_refresh_dns(&state, &uri, req).await
}

/// Disconnect a container from a network, then refresh its host DNS entry.
///
/// # Errors
///
/// Returns an error if VM readiness fails or proxying to guest dockerd fails.
#[tracing::instrument(
    name = "docker.network.disconnect",
    skip(state, req),
    fields(uri = %uri, utility_vm = "native"),
    err
)]
pub async fn network_disconnect(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    req: Request<Body>,
) -> Result<Response> {
    proxy_then_refresh_dns(&state, &uri, req).await
}

/// Buffers the request body (to learn which container is affected), proxies
/// the operation, and on success re-derives the container's DNS entry from a
/// fresh inspect.
async fn proxy_then_refresh_dns(
    state: &AppState,
    uri: &Uri,
    req: Request<Body>,
) -> Result<Response> {
    let (mut parts, body) = req.into_parts();
    let body_bytes = http_body_util::BodyExt::collect(body)
        .await
        .map_err(|e| DockerError::Server(format!("failed to read body: {e}")))?
        .to_bytes();
    let mut request = NetworkRequest::parse(&body_bytes)?;
    let orphan_disconnect = uri.path().ends_with("/disconnect")
        && request
            .options
            .get("Force")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    let lifecycle = match Lifecycle::acquire(state, &request.container).await {
        Ok(lifecycle) => Some(lifecycle),
        // Docker can remove an orphan endpoint after its container disappears.
        Err(DockerError::ContainerNotFound(_)) if orphan_disconnect => None,
        Err(error) => return Err(error),
    };
    let body = if let Some(lifecycle) = &lifecycle {
        request.container.clone_from(&lifecycle.id);
        serde_json::to_vec(&request)
            .map_err(|error| DockerError::Server(format!("encode network request: {error}")))?
            .into()
    } else {
        body_bytes
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let response =
        proxy_to_system_vm(state, uri, Request::from_parts(parts, Body::from(body))).await?;

    if response.status().is_success() {
        if let Some(lifecycle) = &lifecycle {
            refresh_container_dns(state, &lifecycle.id).await?;
        }
    }

    Ok(response)
}

/// Re-derives a container's host DNS entry from a fresh inspect.
///
/// Runtime replacement withdraws the previous address when the last network disappears.
async fn refresh_container_dns(state: &AppState, token: &str) -> Result<()> {
    let inspect = crate::guest_query::inspect_container(state.proxy.client(), token).await?;
    state
        .runtime
        .register_container_dns(&inspect.metadata)
        .await;
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct NetworkRequest {
    #[serde(rename = "Container")]
    container: String,
    #[serde(flatten)]
    options: serde_json::Map<String, serde_json::Value>,
}

impl NetworkRequest {
    fn parse(body: &[u8]) -> Result<Self> {
        let request: Self = serde_json::from_slice(body).map_err(|error| {
            DockerError::InvalidParameter(format!("invalid network request: {error}"))
        })?;
        if request.container.is_empty() {
            return Err(DockerError::InvalidParameter(
                "Container must not be empty".into(),
            ));
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_body_requires_a_nonempty_container() {
        for body in [
            br#"{"Force":true}"#.as_slice(),
            br#"{"Container":""}"#,
            br#"{"Container":3}"#,
            b"not json",
        ] {
            assert!(NetworkRequest::parse(body).is_err());
        }
    }
}
