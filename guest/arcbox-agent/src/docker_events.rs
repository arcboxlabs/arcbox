//! Docker container lifecycle event listener.
//!
//! Subscribes to container lifecycle and network connect/disconnect events
//! through `/var/run/docker.sock` before reconciling running containers.
//! Buffered events keep the guest DNS registry in sync, mirror
//! host-address-pinned port publishes for the inbound relay
//! (`publish_mirror`), and serve each container's domain over HTTP and
//! HTTPS without a port (`domains`).

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;

use anyhow::Context;
use arcbox_constants::dns::Domain;
use arcbox_dns::{container::ContainerMetadata, registry::Registry};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::dns_server::GuestDnsServer;
use crate::domains::{ContainerFacts, DomainRoutes};
use crate::publish_mirror::{PinnedPublish, PublishMirror, pinned_publishes};

const DOCKER_SOCK: &str = "/var/run/docker.sock";
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

struct InspectedContainer {
    metadata: ContainerMetadata,
    facts: Option<ContainerFacts>,
    publishes: Vec<PinnedPublish>,
}

impl InspectedContainer {
    async fn load(socket: &Path, id: &str, domain: &Domain) -> anyhow::Result<Self> {
        let info = docker_get_at(socket, &format!("/containers/{id}/json")).await?;
        let metadata = ContainerMetadata::from_inspect(&info)
            .with_context(|| format!("Docker inspect for {id} omitted container identity"))?;
        anyhow::ensure!(metadata.id == id, "Docker inspect ID does not match {id}");
        Ok(Self {
            metadata,
            facts: ContainerFacts::from_inspect(&info, domain),
            publishes: pinned_publishes(&info),
        })
    }
}

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
    let socket = Path::new(DOCKER_SOCK);
    let Some(mut lines) =
        subscribe_and_reconcile(socket, cancel, reconcile_existing(sync, socket)).await?
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

        handle_event(sync, trimmed).await?;
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

/// Validates a complete snapshot before replacing DNS identities and reconciling routes.
async fn reconcile_existing(sync: &mut ContainerSync<'_>, socket: &Path) -> anyhow::Result<()> {
    let containers = docker_get_at(socket, "/containers/json").await?;
    let arr = containers
        .as_array()
        .context("Docker container list must be an array")?;
    let ids: BTreeSet<_> = arr
        .iter()
        .map(|entry| {
            entry["Id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Docker container list omitted ID")
        })
        .collect::<anyhow::Result<_>>()?;
    let mut snapshot = Vec::with_capacity(ids.len());
    for id in &ids {
        snapshot.push(InspectedContainer::load(socket, id, sync.dns.domain()).await?);
    }
    let mut facts = Vec::new();
    let mut untracked = Vec::new();
    for container in &mut snapshot {
        match container.facts.take() {
            Some(fact) => facts.push(fact),
            None => untracked.push(container.metadata.id.clone()),
        }
    }
    sync.domains.reconcile(facts).await?;

    let registry = sync.dns.containers();
    let absent = {
        let mut registry = registry.write().expect("DNS registry lock poisoned");
        // Resource owners are namespaced; Docker's complete container IDs contain no colon.
        let absent: Vec<_> = registry
            .owners()
            .filter(|owner| !owner.contains(':') && !ids.contains(owner))
            .map(str::to_owned)
            .collect();
        for container in &snapshot {
            sync_dns(&mut registry, sync.dns.domain(), &container.metadata)?;
        }
        // Disable every absent owner before a failed external cleanup can interrupt reconciliation.
        for id in &absent {
            registry.replace(id.clone(), Vec::new(), None)?;
        }
        absent
    };
    for id in absent {
        forget_container(sync, &id).await?;
    }
    for id in untracked {
        sync.domains.forget(&id).await?;
    }
    for container in snapshot {
        sync.mirror
            .apply(&container.metadata.id, &container.publishes)
            .await?;
    }

    tracing::info!(count = ids.len(), "reconciled existing containers");
    Ok(())
}

/// Handles a single Docker event JSON line.
async fn handle_event(sync: &mut ContainerSync<'_>, json_str: &str) -> anyhow::Result<()> {
    let event: serde_json::Value = serde_json::from_str(json_str)?;

    if let Some(id) = network_container_id(&event)? {
        return register_container_by_id(sync, Path::new(DOCKER_SOCK), id).await;
    }
    if event["Type"].as_str() != Some("container") {
        return Ok(());
    }

    let action = event["Action"].as_str().unwrap_or_default();
    let id_full = event["Actor"]["ID"].as_str().unwrap_or_default();

    match action {
        "start" | "rename" => {
            anyhow::ensure!(!id_full.is_empty(), "container {action} event omitted ID");
            register_container_by_id(sync, Path::new(DOCKER_SOCK), id_full).await?;
        }
        "die" | "destroy" => {
            anyhow::ensure!(!id_full.is_empty(), "container {action} event omitted ID");
            forget_container(sync, id_full).await?;
        }
        _ => {}
    }

    Ok(())
}

fn network_container_id(event: &serde_json::Value) -> anyhow::Result<Option<&str>> {
    if event["Type"].as_str() != Some("network")
        || !matches!(event["Action"].as_str(), Some("connect" | "disconnect"))
    {
        return Ok(None);
    }
    let id = event
        .pointer("/Actor/Attributes/container")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .context("network connect/disconnect event omitted container ID")?;
    Ok(Some(id))
}

async fn forget_container(sync: &mut ContainerSync<'_>, id: &str) -> anyhow::Result<()> {
    let registry = sync.dns.containers();
    // An unavailable owner remains enumerable until every external cleanup succeeds.
    registry
        .write()
        .expect("DNS registry lock poisoned")
        .replace(id.to_owned(), Vec::new(), None)?;
    sync.domains.forget(id).await?;
    sync.mirror.remove(id).await?;
    registry
        .write()
        .expect("DNS registry lock poisoned")
        .remove(id);
    Ok(())
}

/// Inspects a container and registers its name + IP in the DNS server,
/// mirrors its host-address-pinned publishes for the inbound relay, and
/// hands it to the domain routes.
async fn register_container_by_id(
    sync: &mut ContainerSync<'_>,
    socket: &Path,
    id: &str,
) -> anyhow::Result<()> {
    let container = InspectedContainer::load(socket, id, sync.dns.domain()).await?;
    let tracked = container.facts.is_some();
    sync.domains.update(id, container.facts).await?;
    sync_dns(
        &mut sync
            .dns
            .containers()
            .write()
            .expect("DNS registry lock poisoned"),
        sync.dns.domain(),
        &container.metadata,
    )?;
    if !tracked {
        sync.domains.forget(id).await?;
    }
    sync.mirror.apply(id, &container.publishes).await
}

fn sync_dns(
    registry: &mut Registry,
    domain: &Domain,
    metadata: &ContainerMetadata,
) -> anyhow::Result<()> {
    let claims = metadata.claims(domain, "container");
    for warning in claims.warnings {
        tracing::warn!(container_id = %metadata.id, %warning, "rejected container DNS name");
    }
    let address = metadata.primary_ipv4().map(std::net::IpAddr::V4);
    registry.replace(metadata.id.clone(), claims.claims, address)?;
    Ok(())
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
