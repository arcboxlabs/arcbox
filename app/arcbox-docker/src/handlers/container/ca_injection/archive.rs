//! Docker archive operations preserve the existing bundle's metadata and roots.

use std::{path::Path, time::Duration};

use axum::{
    body::to_bytes,
    http::{Method, StatusCode},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use super::super::guest::{Container, MAX_ARCHIVE_SIZE, response_error};
use crate::error::{DockerError, Result};

/// Delay before the first retry of an archive request dockerd answered with 500.
pub(super) const BUSY_RETRY_DELAY: Duration = Duration::from_millis(5);
/// Retries after the first attempt; the delays double, so the total wait is 155 ms.
pub(super) const BUSY_RETRIES: u32 = 5;

impl Container<'_> {
    /// Sends one archive request, retrying a 500 answer with backoff.
    ///
    /// dockerd mounts the container rootfs for every archive request and
    /// releases it a few milliseconds after it replies. The guest's overlay
    /// mounts use `index=on`, so a second mount of the same upperdir inside
    /// that window fails with `EBUSY` ("upperdir is in-use"), and dockerd
    /// answers 500. A HEAD answer has no body to tell this apart from other
    /// server errors, so every 500 is retried. Retrying is safe because each
    /// archive request is idempotent: HEAD and GET only read, and PUT extracts
    /// the same tar again. Measured busy windows closed within 17 ms
    /// (docs/logs/2026-10-30-ca-injection-overlay-busy.md).
    async fn archive(&self, method: Method, query: &str, body: Bytes) -> Result<Response> {
        let suffix = format!("archive?{query}");
        let mut delay = BUSY_RETRY_DELAY;
        for _ in 0..BUSY_RETRIES {
            let response = self.request(method.clone(), &suffix, body.clone()).await?;
            if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
                return Ok(response);
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        self.request(method, &suffix, body).await
    }

    /// Check every component: a symlink can redirect a bundle into a caller mount.
    pub async fn has_no_symlink(&self, path: &Path) -> Result<bool> {
        for component in path.ancestors().filter(|path| *path != Path::new("/")) {
            let path = utf8_percent_encode(
                component
                    .to_str()
                    .ok_or_else(|| DockerError::Server("invalid container path".into()))?,
                NON_ALPHANUMERIC,
            );
            let response = self
                .archive(Method::HEAD, &format!("path={path}"), Bytes::new())
                .await?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(false);
            }
            if !response.status().is_success() {
                return Err(response_error(response).await);
            }
            let value = response
                .headers()
                .get("x-docker-container-path-stat")
                .ok_or_else(|| {
                    DockerError::Server("Docker archive response has no path metadata".into())
                })?;
            let data = STANDARD
                .decode(value.as_bytes())
                .map_err(|e| DockerError::Server(format!("invalid Docker path metadata: {e}")))?;
            #[derive(Deserialize)]
            struct PathStat {
                mode: u32,
            }
            let stat: PathStat = serde_json::from_slice(&data)
                .map_err(|e| DockerError::Server(format!("invalid Docker path metadata: {e}")))?;
            if stat.mode & (1 << 27) != 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn reconcile_bundle(&self, path: &str, pem: &[u8]) -> Result<()> {
        let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC);
        let response = self
            .archive(Method::GET, &format!("path={encoded}"), Bytes::new())
            .await?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        let bytes = to_bytes(response.into_body(), MAX_ARCHIVE_SIZE)
            .await
            .map_err(|e| {
                DockerError::GuestUnavailable(format!("failed to read container CA archive: {e}"))
            })?;
        let Some(bytes) = super::bundle::reconcile_archive(&bytes, Path::new(path), pem)? else {
            return Ok(());
        };
        let parent = path.rsplit_once('/').expect("bundle paths are absolute").0;
        let parent = utf8_percent_encode(parent, NON_ALPHANUMERIC);
        // copyUIDGID=true uses Config.User instead of preserving the archive owner.
        let response = self
            .archive(
                Method::PUT,
                &format!("path={parent}&noOverwriteDirNonDir=true"),
                bytes.into(),
            )
            .await?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        Ok(())
    }
}
