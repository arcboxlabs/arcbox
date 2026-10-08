//! Docker container lifecycle event listener.
//!
//! Connects to `/var/run/docker.sock`, performs initial reconciliation of
//! running containers, then subscribes to container lifecycle and network
//! connect/disconnect events to keep the guest DNS server registry in sync, to
//! mirror host-address-pinned port publishes for the inbound relay
//! (`publish_mirror`), and to serve each container's domain over HTTP and
//! HTTPS without a port (`domains`).

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::dns_server::GuestDnsServer;
use crate::domains::{ContainerFacts, DomainRoutes, primary_ipv4};
use crate::publish_mirror::{PublishMirror, pinned_publishes};

const DOCKER_SOCK: &str = "/var/run/docker.sock";
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// What the watcher keeps in sync for each container.
pub struct ContainerSync<'a> {
    pub dns: &'a GuestDnsServer,
    pub mirror: PublishMirror,
    pub domains: DomainRoutes,
}

/// Runs initial reconciliation then watches Docker events indefinitely.
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
                tracing::warn!(error = %e, "docker event listener disconnected, retrying");
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
        }
    }
}

/// Single connection lifecycle: reconcile + event stream.
async fn run_once(sync: &mut ContainerSync<'_>, cancel: &CancellationToken) -> anyhow::Result<()> {
    // Phase 1: list all running containers and register them.
    if let Err(e) = reconcile_existing(sync).await {
        tracing::warn!(error = %e, "initial container reconciliation failed");
    }

    // Network events carry container IDs in Actor.Attributes, not Actor.ID.
    let stream = UnixStream::connect(DOCKER_SOCK).await?;
    let (reader, mut writer) = tokio::io::split(stream);

    let request = "GET /events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%5D%7D HTTP/1.1\r\n\
                   Host: localhost\r\n\
                   Connection: keep-alive\r\n\r\n";
    writer.write_all(request.as_bytes()).await?;

    let mut lines = BufReader::new(reader).lines();

    // Skip HTTP response headers.
    loop {
        let Some(line) = read_line_or_cancel(&mut lines, cancel).await? else {
            return Ok(());
        };
        if line.is_empty() {
            break; // end of headers
        }
    }

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
        match ContainerFacts::from_inspect(&info) {
            Some(facts) => sync.domains.track(facts),
            None => sync.domains.forget(full_id),
        }
        sync.mirror.apply(full_id, &pinned_publishes(&info)).await?;
    }

    sync_dns(sync.dns, &info).await;
    Ok(())
}

async fn sync_dns(dns: &GuestDnsServer, info: &serde_json::Value) {
    let name = info["Name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('/');
    if name.is_empty() {
        return;
    }

    // Extract compose metadata from labels for hierarchical DNS names.
    let compose = info["Config"]["Labels"]
        .as_object()
        .and_then(crate::dns::extract_compose_info);

    // Register the container name and any compose aliases.
    // The container name is the owner — shared aliases (like compose
    // service names) are ref-counted per owner in the DNS server.
    let aliases = crate::dns::collect_aliases(name, compose.as_ref());
    let Some(ip) = primary_ipv4(info) else {
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
    let stream = UnixStream::connect(DOCKER_SOCK).await?;
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
        .map_or(0, |p| p + 4);
    let body = &buf[body_start..];

    Ok(serde_json::from_slice(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_changes_reinspect_the_container_instead_of_the_network() {
        for action in ["connect", "disconnect"] {
            let mut event = serde_json::json!({
                "Type": "network", "Action": action,
                "Actor": { "ID": "network-id", "Attributes": { "container": "container-id" } }
            });
            assert_eq!(network_container_id(&event), Some("container-id"));
            event["Action"] = "destroy".into();
            assert_eq!(network_container_id(&event), None);
            event["Action"] = action.into();
            event["Actor"]["Attributes"]["container"] = "".into();
            assert_eq!(network_container_id(&event), None);
        }
    }

    #[tokio::test]
    async fn disconnecting_the_last_network_removes_only_that_containers_dns_ownership() {
        let dns = GuestDnsServer::new(CancellationToken::new());
        let mut inspect = serde_json::json!({
            "Name": "/web-1",
            "Config": { "Labels": { "com.docker.compose.project": "demo", "com.docker.compose.service": "web" } },
            "NetworkSettings": { "Networks": { "app": { "IPAddress": "172.18.0.2" } } }
        });
        sync_dns(&dns, &inspect).await;
        dns.register_container("web.demo", "web-2", "172.18.0.3".parse().unwrap())
            .await;
        inspect["NetworkSettings"]["Networks"] = serde_json::json!({});
        sync_dns(&dns, &inspect).await;
        let entries = dns.containers();
        let entries = entries.read().await;
        assert!(!entries.contains_key("web-1"));
        assert_eq!(
            entries.get("web.demo"),
            Some(&"172.18.0.3".parse().unwrap())
        );
    }
}
