//! Inject the current public CA before a container starts, without executing image code.

mod archive;
mod bundle;
#[cfg(test)]
mod test_support;

use super::{guest::Container, start::Start};

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[cfg(target_os = "macos")]
use arcbox_local_ca::PublicAuthority;
use axum::{
    body::{Body, to_bytes},
    http::{StatusCode, header},
    response::Response,
};
use serde::Deserialize;

use crate::{
    api::AppState,
    error::{DockerError, Result},
    proxy::ActivityClass,
};

const OPT_OUT: &str = "dev.arcbox.add-ca-certificates";
const BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/var/lib/ca-certificates/ca-bundle.pem",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];
const UNAVAILABLE: &str =
    "ArcBox CA injection is pending: the current, valid public CA is not published.";

pub(super) async fn after_create(state: &AppState, response: Response) -> Result<Response> {
    if response.status() != StatusCode::CREATED {
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1024 * 1024).await.map_err(|e| {
        DockerError::GuestUnavailable(format!("failed to read container creation: {e}"))
    })?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| DockerError::Server(format!("invalid container creation response: {e}")))?;
    let id = value["Id"]
        .as_str()
        .ok_or_else(|| DockerError::Server("container creation response has no ID".into()))?;
    let result = async {
        let start = Start::acquire(state, id).await?;
        reconcile(state, &start.id).await
    }
    .await;
    let warnings = match result {
        Ok(warnings) => warnings,
        Err(error) => vec![format!(
            "ArcBox CA injection failed; container start will retry: {error}"
        )],
    };
    if warnings.is_empty() {
        return Ok(Response::from_parts(parts, Body::from(bytes)));
    }
    if value["Warnings"].is_null() {
        value["Warnings"] = serde_json::json!([]);
    }
    let target = value["Warnings"]
        .as_array_mut()
        .ok_or_else(|| DockerError::Server("invalid container creation warnings".into()))?;
    target.extend(warnings.into_iter().map(serde_json::Value::String));
    parts.headers.remove(header::CONTENT_LENGTH);
    Ok(Response::from_parts(parts, Body::from(value.to_string())))
}

pub(super) fn report_warnings(response: &mut Response, warnings: Vec<String>) {
    for warning in warnings {
        tracing::warn!(%warning, "container CA injection incomplete");
        // These messages contain only static ASCII text and fixed bundle paths.
        response.headers_mut().append(
            header::WARNING,
            format!("299 ArcBox \"{warning}\"")
                .parse()
                .expect("CA warnings are valid header text"),
        );
    }
}

pub(super) async fn reconcile(state: &AppState, id: &str) -> Result<Vec<String>> {
    let tls_dir = state.runtime.config().data_dir.join("tls");
    let published = tokio::fs::read(tls_dir.join("ca.pem")).await;
    if matches!(&published, Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
        return Ok(vec![UNAVAILABLE.into()]);
    }
    let result = reconcile_with_root(state, id, move || published_root(&tls_dir, published?)).await;
    crate::proxy::invalidate_on_guest_error(state, result)
}

async fn reconcile_with_root<F>(state: &AppState, id: &str, root: F) -> Result<Vec<String>>
where
    F: FnOnce() -> Result<Option<Vec<u8>>> + Send + 'static,
{
    super::super::ensure_system_vm_ready(state, ActivityClass::Active).await?;
    let _lease = state.proxy.activity_lease();
    let container = Container { state, id };
    let body = container.inspect().await?;
    let metadata: Metadata = serde_json::from_slice(&body)
        .map_err(|e| DockerError::Server(format!("invalid container inspect response: {e}")))?;
    if metadata.disabled() {
        return Ok(Vec::new());
    }
    if let Some(reason) = metadata.unsupported() {
        return Ok(vec![reason.into()]);
    }
    let pem = tokio::task::spawn_blocking(root)
        .await
        .map_err(|e| DockerError::Server(format!("failed to inspect public CA: {e}")))??;
    let Some(pem) = pem else {
        return Ok(vec![UNAVAILABLE.into()]);
    };
    for mount in &metadata.mounts {
        if !container
            .has_no_symlink(Path::new(&mount.destination))
            .await?
        {
            return Ok(vec!["ArcBox CA injection skipped: a container mount has an unresolved or symbolic path.".into()]);
        }
    }
    let mut updated = 0;
    let mut warnings = Vec::new();
    for path in BUNDLES {
        if metadata.mounted(Path::new(path)) {
            warnings.push(format!(
                "ArcBox CA injection skipped caller-mounted bundle {path}."
            ));
            continue;
        }
        if container.has_no_symlink(Path::new(path)).await? {
            container.reconcile_bundle(path, &pem).await?;
            updated += 1;
        }
    }
    if updated == 0 {
        warnings.push(
            "ArcBox CA injection skipped: no supported regular PEM system bundle was found.".into(),
        );
    }
    Ok(warnings)
}

#[cfg(target_os = "macos")]
fn published_root(tls_dir: &Path, pem: Vec<u8>) -> Result<Option<Vec<u8>>> {
    let authority = PublicAuthority::from_pem(&pem)
        .map_err(|e| DockerError::Server(format!("invalid published ArcBox CA: {e}")))?;
    let current = arcbox_local_ca::KeychainAuthority::existing_certificate(tls_dir)
        .map_err(|e| DockerError::Server(format!("failed to inspect current ArcBox CA: {e}")))?;
    Ok((current.as_deref() == Some(authority.der())
        && authority
            .validity()
            .is_valid_at(std::time::SystemTime::now().into()))
    .then_some(pem))
}

#[cfg(not(target_os = "macos"))]
fn published_root(_tls_dir: &Path, _pem: Vec<u8>) -> Result<Option<Vec<u8>>> {
    Ok(None)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Metadata {
    config: Config,
    host_config: HostConfig,
    state: State,
    mounts: Vec<Mount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    labels: Option<HashMap<String, String>>,
    env: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HostConfig {
    readonly_rootfs: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    running: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    destination: PathBuf,
}

impl Metadata {
    fn disabled(&self) -> bool {
        self.config
            .labels
            .as_ref()
            .and_then(|labels| labels.get(OPT_OUT))
            .is_some_and(|value| value == "false")
    }
    fn unsupported(&self) -> Option<&'static str> {
        if self.state.running {
            return Some("ArcBox CA injection skipped: the container is already running.");
        }
        if self.host_config.readonly_rootfs {
            return Some("ArcBox CA injection skipped: the container rootfs is read-only.");
        }
        if self.config.env.iter().flatten().any(|value| {
            value.split_once('=').is_some_and(|(name, _)| {
                matches!(
                    name,
                    "SSL_CERT_FILE"
                        | "SSL_CERT_DIR"
                        | "CURL_CA_BUNDLE"
                        | "REQUESTS_CA_BUNDLE"
                        | "NODE_EXTRA_CA_CERTS"
                        | "JAVA_TOOL_OPTIONS"
                        | "_JAVA_OPTIONS"
                )
            })
        }) {
            return Some(
                "ArcBox CA injection skipped: the container configures a custom certificate environment.",
            );
        }
        None
    }
    fn mounted(&self, path: &Path) -> bool {
        self.mounts
            .iter()
            .any(|mount| path.starts_with(&mount.destination))
    }
}
