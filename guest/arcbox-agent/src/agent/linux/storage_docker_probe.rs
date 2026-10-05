//! A local Docker lifecycle check with an owned image and container.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use arcbox_constants::paths::DOCKER_API_UNIX_SOCKET;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::UnixStream;

const API_TIMEOUT: Duration = Duration::from_secs(30);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(120);
const RESPONSE_LIMIT: usize = 1024 * 1024;

pub(super) async fn verify(busybox: &Path) -> Result<String> {
    let busybox = busybox.to_owned();
    let archive = tokio::task::spawn_blocking(move || rootfs_archive(&busybox)).await??;
    let name = format!("arcbox-storage-check-{}", uuid::Uuid::new_v4());
    let image = format!("{name}:probe");
    let result = lifecycle(&name, &image, archive).await;
    // Names belong to this operation even if an HTTP timeout hides whether
    // Docker completed creation. Attempt both removals before reporting.
    let container_cleanup = remove(&format!("/containers/{name}?force=true")).await;
    let image_cleanup = remove(&format!("/images/{image}")).await;
    match (result, container_cleanup, image_cleanup) {
        (Ok(()), Ok(()), Ok(())) => Ok("Docker image import, container create/start/write/sync/read/wait, container removal, and image removal succeeded".into()),
        (result, container, image) => bail!(
            "Docker probe {name}: lifecycle {}; container cleanup {}; image cleanup {}",
            outcome(&result), outcome(&container), outcome(&image)
        ),
    }
}

fn outcome(result: &Result<()>) -> String {
    match result {
        Ok(()) => "succeeded".into(),
        Err(error) => format!("failed: {error:#}"),
    }
}

fn rootfs_archive(busybox: &Path) -> Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());
    archive
        .append_path_with_name(busybox, "bin/busybox")
        .context("archive the guest's static busybox")?;
    archive
        .into_inner()
        .context("finish the probe rootfs archive")
}

async fn lifecycle(name: &str, image: &str, archive: Vec<u8>) -> Result<()> {
    let imported = request(
        Method::POST,
        &format!("/images/create?fromSrc=-&repo={name}&tag=probe"),
        "application/x-tar",
        archive,
        IMPORT_TIMEOUT,
    )
    .await?
    .success()?;
    verify_import(&imported)?;
    let config = serde_json::json!({
        "Image": image,
        "Cmd": ["/bin/busybox", "sh", "-ec", "printf 'arcbox-storage-check\\n' > /probe-data; /bin/busybox sync -d /probe-data; read value < /probe-data; test \"$value\" = arcbox-storage-check; /bin/busybox rm /probe-data; /bin/busybox sync -f /"],
        "HostConfig": { "NetworkMode": "none", "RestartPolicy": { "Name": "no" } }
    });
    request(
        Method::POST,
        &format!("/containers/create?name={name}"),
        "application/json",
        serde_json::to_vec(&config)?,
        API_TIMEOUT,
    )
    .await?
    .success()?;
    request(
        Method::POST,
        &format!("/containers/{name}/start"),
        "application/json",
        Vec::new(),
        API_TIMEOUT,
    )
    .await?
    .success()?;
    let waited = request(
        Method::POST,
        &format!("/containers/{name}/wait?condition=not-running"),
        "application/json",
        Vec::new(),
        API_TIMEOUT,
    )
    .await?
    .success()?;
    verify_exit(&waited)
}

#[derive(Deserialize)]
struct ImportEvent {
    error: Option<String>,
}

fn verify_import(body: &[u8]) -> Result<()> {
    for event in serde_json::Deserializer::from_slice(body).into_iter::<ImportEvent>() {
        if let Some(error) = event.context("decode Docker import progress")?.error {
            bail!("Docker import failed: {error}");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct WaitResult {
    #[serde(rename = "StatusCode")]
    status: i64,
    #[serde(rename = "Error")]
    error: Option<WaitError>,
}

#[derive(Deserialize)]
struct WaitError {
    #[serde(rename = "Message")]
    message: String,
}

fn verify_exit(body: &[u8]) -> Result<()> {
    let waited: WaitResult = serde_json::from_slice(body).context("decode Docker wait result")?;
    if let Some(error) = waited.error {
        bail!("Docker wait failed: {}", error.message);
    }
    if waited.status != 0 {
        bail!("probe container exited with status {}", waited.status);
    }
    Ok(())
}

async fn remove(path: &str) -> Result<()> {
    let response = request(
        Method::DELETE,
        path,
        "application/json",
        Vec::new(),
        API_TIMEOUT,
    )
    .await?;
    if response.status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    response.success().map(drop)
}

struct Response {
    status: StatusCode,
    body: Vec<u8>,
}

impl Response {
    fn success(self) -> Result<Vec<u8>> {
        if !self.status.is_success() {
            bail!(
                "Docker returned HTTP {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            );
        }
        Ok(self.body)
    }
}

async fn request(
    method: Method,
    path: &str,
    content_type: &'static str,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<Response> {
    tokio::time::timeout(timeout, async {
        let stream = UnixStream::connect(DOCKER_API_UNIX_SOCKET).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "Docker storage probe connection closed");
            }
        });
        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::CONNECTION, "close")
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from(body)))?;
        let mut response = sender.send_request(request).await?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(frame) = response.frame().await {
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                if body.len() + data.len() > RESPONSE_LIMIT {
                    bail!("Docker response exceeds {RESPONSE_LIMIT} bytes");
                }
                body.extend_from_slice(data);
            }
        }
        Ok(Response { status, body })
    })
    .await
    .with_context(|| format!("Docker request {path} timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_http_import_can_still_contain_a_docker_error() {
        assert!(
            verify_import(b"{\"status\":\"loading\"}\n{\"error\":\"disk read-only\"}\n").is_err()
        );
        assert!(verify_import(b"{\"status\":\"sha256:abc\"}\n").is_ok());
    }

    #[test]
    fn wait_requires_successful_exit_without_an_engine_error() {
        assert!(verify_exit(br#"{"StatusCode":0}"#).is_ok());
        assert!(verify_exit(br#"{"StatusCode":1}"#).is_err());
        assert!(verify_exit(br#"{"StatusCode":0,"Error":{"Message":"write failed"}}"#).is_err());
        assert!(verify_exit(b"{}").is_err());
    }
}
