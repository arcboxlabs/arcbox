//! Docker container lifecycle event listener.
//!
//! Subscribes to container lifecycle and network connect/disconnect events
//! through `/var/run/docker.sock` before reconciling running containers.
//! Buffered events keep the guest DNS registry in sync, mirror
//! host-address-pinned port publishes for the inbound relay
//! (`publish_mirror`), and serve each container's domain over HTTP and
//! HTTPS without a port (`domains`).

use std::future::Future;
use std::path::Path;

use anyhow::Context;
use arcbox_dns::container::ContainerMetadata;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::dns_server::GuestDnsServer;
use crate::domains::{ContainerFacts, DomainRoutes};
use crate::publish_mirror::{PublishMirror, pinned_publishes};

const DOCKER_SOCK: &str = "/var/run/docker.sock";
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// What the watcher keeps in sync for each container.
pub struct ContainerSync<'a> {
    pub dns: &'a GuestDnsServer,
    pub mirror: PublishMirror,
    pub domains: DomainRoutes,
}

/// Subscribes before reconciliation, then watches Docker events indefinitely.
///
/// Retries connection with backoff until the Docker socket appears.
/// Blocks until `cancel` is triggered.
pub async fn reconcile_and_watch(mut sync: ContainerSync<'_>, cancel: CancellationToken) {
    loop {
        // Wait for socket to exist.
        while !Path::new(DOCKER_SOCK).exists() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RETRY_DELAY) => {}
            }
        }

        match run_once(&mut sync, &cancel).await {
            Ok(()) => return, // cancelled
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "docker event listener disconnected, retrying");
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
        }
    }
}

/// Subscribes before reconciliation so changes during the listing stay buffered.
async fn run_once(sync: &mut ContainerSync<'_>, cancel: &CancellationToken) -> anyhow::Result<()> {
    let Some(mut lines) =
        subscribe_and_reconcile(Path::new(DOCKER_SOCK), cancel, reconcile_existing(sync)).await?
    else {
        return Ok(());
    };

    // Process chunked event stream.
    loop {
        let Some(line) = read_line_or_cancel(&mut lines, cancel).await? else {
            return Ok(());
        };

        // Docker chunked encoding: hex size line, then JSON line.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        if let Err(e) = handle_event(sync, trimmed).await {
            tracing::debug!(error = %e, "failed to process docker event");
        }
    }
}

async fn subscribe_and_reconcile(
    socket: &Path,
    cancel: &CancellationToken,
    reconcile: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<Option<tokio::io::Lines<BufReader<tokio::io::ReadHalf<UnixStream>>>>> {
    // Network events carry container IDs in Actor.Attributes, not Actor.ID.
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = tokio::io::split(stream);

    let request = "GET /events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%5D%7D HTTP/1.1\r\n\
                   Host: localhost\r\n\
                   Connection: keep-alive\r\n\r\n";
    writer.write_all(request.as_bytes()).await?;

    let mut lines = BufReader::new(reader).lines();

    let Some(status) = read_line_or_cancel(&mut lines, cancel).await? else {
        return Ok(None);
    };
    require_success(&status).context("subscribe to Docker events")?;

    // Docker confirms the subscription before the snapshot request starts.
    loop {
        let Some(line) = read_line_or_cancel(&mut lines, cancel).await? else {
            return Ok(None);
        };
        if line.is_empty() {
            break; // end of headers
        }
    }

    reconcile.await?;
    Ok(Some(lines))
}

/// Reads one line, returning `None` if cancelled.
async fn read_line_or_cancel(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<UnixStream>>>,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<String>> {
    tokio::select! {
        () = cancel.cancelled() => Ok(None),
        result = lines.next_line() => {
            match result? {
                Some(line) => Ok(Some(line)),
                None => anyhow::bail!("event stream ended"),
            }
        }
    }
}

/// Lists running containers and registers them in the DNS server.
async fn reconcile_existing(sync: &mut ContainerSync<'_>) -> anyhow::Result<()> {
    let containers = docker_get("/containers/json").await?;
    let arr = containers
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array"))?;

    for c in arr {
        let Some(id) = c["Id"].as_str() else {
            continue;
        };
        let short_id = &id[..12.min(id.len())];
        if let Err(e) = register_container_by_id(sync, short_id).await {
            tracing::debug!(id = short_id, error = %e, "failed to register container");
        }
    }

    tracing::info!(count = arr.len(), "reconciled existing containers");
    Ok(())
}

/// Handles a single Docker event JSON line.
async fn handle_event(sync: &mut ContainerSync<'_>, json_str: &str) -> anyhow::Result<()> {
    let event: serde_json::Value = serde_json::from_str(json_str)?;

    if let Some(id) = network_container_id(&event) {
        return register_container_by_id(sync, id).await;
    }
    if event["Type"].as_str() != Some("container") {
        return Ok(());
    }

    let action = event["Action"].as_str().unwrap_or_default();
    let id_full = event["Actor"]["ID"].as_str().unwrap_or_default();
    let id = &id_full[..12.min(id_full.len())];

    match action {
        "start" => {
            register_container_by_id(sync, id).await?;
        }
        "die" | "destroy" => {
            // Deregister this container's ownership of all its aliases.
            // Shared aliases (compose service names) are only removed from
            // DNS when no other replica still references them.
            let attrs = &event["Actor"]["Attributes"];
            let name = attrs["name"].as_str().unwrap_or_default();
            if !name.is_empty() {
                let compose = attrs.as_object().and_then(crate::dns::extract_compose_info);
                for alias in crate::dns::collect_aliases(name, compose.as_ref()) {
                    sync.dns.deregister_container(&alias, name).await;
                }
            }
            if !id_full.is_empty() {
                sync.domains.forget(id_full);
                sync.mirror.remove(id_full).await?;
            }
        }
        "rename" => {
            // Deregister old name's ownership, register under new name.
            let attrs = &event["Actor"]["Attributes"];
            let old_name = attrs["oldName"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('/');
            if !old_name.is_empty() {
                let compose = attrs.as_object().and_then(crate::dns::extract_compose_info);
                for alias in crate::dns::collect_aliases(old_name, compose.as_ref()) {
                    sync.dns.deregister_container(&alias, old_name).await;
                }
            }
            register_container_by_id(sync, id).await?;
        }
        _ => {}
    }

    Ok(())
}

fn network_container_id(event: &serde_json::Value) -> Option<&str> {
    if event["Type"].as_str() != Some("network")
        || !matches!(event["Action"].as_str(), Some("connect" | "disconnect"))
    {
        return None;
    }
    event
        .pointer("/Actor/Attributes/container")?
        .as_str()
        .filter(|id| !id.is_empty())
}

/// Inspects a container and registers its name + IP in the DNS server,
/// mirrors its host-address-pinned publishes for the inbound relay, and
/// hands it to the domain routes.
async fn register_container_by_id(sync: &mut ContainerSync<'_>, id: &str) -> anyhow::Result<()> {
    let info = docker_get(&format!("/containers/{id}/json")).await?;

    // Keyed by the full ID: the die/destroy events carry that, not the name.
    if let Some(full_id) = info["Id"].as_str() {
        match ContainerFacts::from_inspect(&info, sync.dns.domain()) {
            Some(facts) => sync.domains.track(facts),
            None => sync.domains.forget(full_id),
        }
        sync.mirror.apply(full_id, &pinned_publishes(&info)).await?;
    }

    sync_dns(sync.dns, &info).await;
    Ok(())
}

async fn sync_dns(dns: &GuestDnsServer, info: &serde_json::Value) {
    let Some(metadata) = ContainerMetadata::from_inspect(info) else {
        return;
    };
    let name = metadata.name.as_str();

    // The DNS server tracks shared aliases by container name.
    let aliases = metadata.default_aliases();
    let Some(ip) = metadata.primary_ipv4() else {
        for alias in &aliases {
            dns.deregister_container(alias, name).await;
        }
        return;
    };
    for alias in &aliases {
        dns.register_container(alias, name, ip).await;
    }

    tracing::debug!(name, %ip, aliases = ?aliases, "registered container DNS");
}

/// Performs a GET request to the Docker Engine API via Unix socket.
pub async fn docker_get(path: &str) -> anyhow::Result<serde_json::Value> {
    docker_get_at(Path::new(DOCKER_SOCK), path).await
}

async fn docker_get_at(socket: &Path, path: &str) -> anyhow::Result<serde_json::Value> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = tokio::io::split(stream);

    let request = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
    writer.write_all(request.as_bytes()).await?;
    writer.shutdown().await?;

    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut BufReader::new(reader), &mut buf).await?;

    // Split HTTP response: headers \r\n\r\n body
    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("incomplete Docker HTTP response"))?
        + 4;
    let (headers, body) = buf.split_at(body_start);
    let status = headers
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    require_success(std::str::from_utf8(status)?).with_context(|| format!("GET {path}"))?;

    serde_json::from_slice(body).with_context(|| format!("parse GET {path} response body"))
}

fn require_success(line: &str) -> anyhow::Result<()> {
    let mut words = line.split_ascii_whitespace();
    anyhow::ensure!(
        matches!(words.next(), Some("HTTP/1.0" | "HTTP/1.1")),
        "invalid Docker HTTP response: {line}"
    );
    let status: hyper::StatusCode = words
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing Docker HTTP status"))?
        .parse()?;
    anyhow::ensure!(status.is_success(), "Docker API returned {status}");
    Ok(())
}

#[cfg(test)]
mod tests;
