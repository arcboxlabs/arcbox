//! Strict container queries shared by lifecycle serialization and CA injection.

use axum::{
    body::to_bytes,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;

use crate::{
    api::AppState,
    error::{DockerError, Result},
    proxy,
};

pub(super) const MAX_ARCHIVE_SIZE: usize = 4 * 1024 * 1024;

pub(super) struct Container<'a> {
    pub state: &'a AppState,
    pub id: &'a str,
}

impl Container<'_> {
    pub async fn inspect(&self) -> Result<Bytes> {
        let response = self.request(Method::GET, "json", Bytes::new()).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerError::ContainerNotFound(self.id.into()));
        }
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        to_bytes(response.into_body(), MAX_ARCHIVE_SIZE)
            .await
            .map_err(|e| DockerError::GuestUnavailable(format!("failed to inspect container: {e}")))
    }

    pub async fn request(&self, method: Method, suffix: &str, body: Bytes) -> Result<Response> {
        proxy::invalidate_on_guest_error(
            self.state,
            proxy::proxy_to_guest_pooled(
                self.state.proxy.client(),
                method,
                &format!("/containers/{}/{suffix}", self.id),
                &HeaderMap::new(),
                body,
            )
            .await,
        )
    }
}

pub(super) async fn response_error(response: Response) -> DockerError {
    let status = response.status();
    match to_bytes(response.into_body(), MAX_ARCHIVE_SIZE).await {
        Ok(body) => DockerError::Server(format!(
            "container operation returned {status}: {}",
            String::from_utf8_lossy(&body)
        )),
        Err(error) => {
            DockerError::GuestUnavailable(format!("failed to read container error: {error}"))
        }
    }
}
