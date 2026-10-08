//! Keep container identity stable while serializing operations that can start it.

use axum::http::Uri;
use serde::Deserialize;

use crate::{
    api::AppState,
    error::{DockerError, Result},
    proxy::ActivityClass,
};

pub(super) struct Start {
    pub id: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Start {
    pub async fn acquire(state: &AppState, id: &str) -> Result<Self> {
        super::super::ensure_system_vm_ready(state, ActivityClass::Active).await?;
        let _lease = state.proxy.activity_lease();
        #[derive(Deserialize)]
        struct Identity {
            #[serde(rename = "Id")]
            id: String,
        }
        let body = super::guest::Container { state, id }.inspect().await?;
        let Identity { id } = serde_json::from_slice(&body)
            .map_err(|e| DockerError::Server(format!("invalid container identity: {e}")))?;
        let guard = state.proxy.lock_container_start(&id).await;
        Ok(Self { id, _guard: guard })
    }

    pub fn uri(&self, uri: &Uri) -> Result<Uri> {
        let (prefix, tail) = uri
            .path()
            .split_once("/containers/")
            .ok_or_else(|| DockerError::InvalidParameter("missing container path".into()))?;
        let (_, operation) = tail
            .split_once('/')
            .ok_or_else(|| DockerError::InvalidParameter("missing container operation".into()))?;
        let mut path = format!("{prefix}/containers/{}/{operation}", self.id);
        if let Some(query) = uri.query() {
            path.push('?');
            path.push_str(query);
        }
        let mut parts = uri.clone().into_parts();
        parts.path_and_query =
            Some(path.parse().map_err(|e| {
                DockerError::Server(format!("invalid canonical container URI: {e}"))
            })?);
        Uri::from_parts(parts)
            .map_err(|e| DockerError::Server(format!("invalid canonical container URI: {e}")))
    }
}
