//! Keep container identity stable while serializing guest mutations and host publication.

use axum::http::Uri;
use serde::Deserialize;

use crate::{
    api::AppState,
    error::{DockerError, Result},
    proxy::{ActivityClass, ActivityLease},
};

pub(in crate::handlers) struct Lifecycle<'a> {
    pub id: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _lease: Option<ActivityLease>,
    _network: arcbox_core::runtime::HostMutationLease<'a>,
}

impl<'a> Lifecycle<'a> {
    pub async fn from_uri(state: &'a AppState, uri: &Uri) -> Result<Self> {
        let token = super::super::extract_container_id(uri)
            .ok_or_else(|| DockerError::InvalidParameter("missing container path".into()))?;
        Self::acquire(state, &token).await
    }

    pub async fn acquire(state: &'a AppState, id: &str) -> Result<Self> {
        super::super::ensure_system_vm_ready(state, ActivityClass::Active).await?;
        let lease = state.proxy.activity_lease();
        let vm = state.runtime.container_network_incarnation()?;
        #[derive(Deserialize)]
        struct Identity {
            #[serde(rename = "Id")]
            id: String,
        }
        let body = crate::proxy::invalidate_on_guest_error(
            state,
            super::guest::Container { state, id }.inspect().await,
        )?;
        let Identity { id } = serde_json::from_slice(&body)
            .map_err(|e| DockerError::Server(format!("invalid container identity: {e}")))?;
        if id.is_empty() {
            return Err(DockerError::Server(
                "invalid container identity: empty Id".into(),
            ));
        }
        let guard = state.runtime.lock_container_lifecycle(&id).await;
        let network = state.runtime.begin_container_mutation(vm).await?;
        Ok(Self {
            id,
            _guard: guard,
            _lease: lease,
            _network: network,
        })
    }

    pub fn uri(&self, uri: &Uri) -> Result<Uri> {
        let (prefix, tail) = uri
            .path()
            .split_once("/containers/")
            .ok_or_else(|| DockerError::InvalidParameter("missing container path".into()))?;
        let suffix = tail.find('/').map_or("", |index| &tail[index..]);
        let mut path = format!("{prefix}/containers/{}{suffix}", self.id);
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
