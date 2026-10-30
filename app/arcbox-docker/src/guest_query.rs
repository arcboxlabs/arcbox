//! Read-only guest dockerd queries shared by the container handlers, the
//! host-networking reconciler, and daemon cold-start recovery.
//!
//! Validate complete snapshots before callers change host networking state.

use crate::error::{DockerError, Result};
use crate::proxy::{GuestHttpClient, proxy_to_guest_pooled};
use arcbox_core::runtime::ContainerNetwork;
use arcbox_dns::container::ContainerMetadata;
use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use http_body_util::BodyExt;
use std::collections::HashSet;

/// A validated container identity and the inspect body used for port bindings.
pub struct ContainerInspect {
    pub metadata: ContainerMetadata,
    pub body: Bytes,
}

/// Fetches and validates an inspect snapshot before callers change host state.
pub async fn inspect_container(client: &GuestHttpClient, token: &str) -> Result<ContainerInspect> {
    let path = format!("/containers/{token}/json");
    let response =
        proxy_to_guest_pooled(client, Method::GET, &path, &HeaderMap::new(), Bytes::new()).await?;
    if !response.status().is_success() {
        return Err(DockerError::Server(format!(
            "guest {path} returned {}",
            response.status()
        )));
    }
    let body = BodyExt::collect(response.into_body())
        .await
        .map_err(|error| DockerError::Server(format!("read {path} body: {error}")))?
        .to_bytes();
    let value = serde_json::from_slice(&body)
        .map_err(|error| DockerError::Server(format!("parse {path} body: {error}")))?;
    let metadata = ContainerMetadata::from_inspect(&value).ok_or_else(|| {
        DockerError::Server(format!(
            "parse {path} body: nonempty Id and Name are required"
        ))
    })?;
    Ok(ContainerInspect { metadata, body })
}

/// Reads every running container before returning a snapshot for host reconciliation.
/// Failed or mismatched inspect responses reject the entire snapshot.
pub async fn running_container_snapshot(client: &GuestHttpClient) -> Result<Vec<ContainerNetwork>> {
    let mut ids: Vec<_> = list_running_container_ids(client)
        .await?
        .into_iter()
        .collect();
    ids.sort_unstable();
    let mut snapshot = Vec::with_capacity(ids.len());
    for id in ids {
        let inspect = inspect_container(client, &id).await?;
        if inspect.metadata.id != id {
            return Err(DockerError::Server(format!(
                "guest inspect ID {} does not match requested {id}",
                inspect.metadata.id
            )));
        }
        snapshot.push(ContainerNetwork {
            bindings: crate::port_bindings::parse_port_bindings(&inspect.body)
                .into_iter()
                .map(Into::into)
                .collect(),
            metadata: inspect.metadata,
        });
    }
    Ok(snapshot)
}

/// Queries guest dockerd for the IDs of currently-running containers.
///
/// Invalid responses return an error so callers cannot treat a partial list as complete.
pub async fn list_running_container_ids(client: &GuestHttpClient) -> Result<HashSet<String>> {
    let response = proxy_to_guest_pooled(
        client,
        Method::GET,
        "/containers/json",
        &HeaderMap::new(),
        Bytes::new(),
    )
    .await?;
    if !response.status().is_success() {
        return Err(DockerError::Server(format!(
            "guest /containers/json returned {}",
            response.status()
        )));
    }
    let body = BodyExt::collect(response.into_body())
        .await
        .map_err(|e| DockerError::Server(format!("read /containers/json body: {e}")))?
        .to_bytes();
    parse_container_ids(&body)
}

/// Extracts full container IDs from a `GET /containers/json` response body.
/// Rejects an invalid listing so reconciliation cannot remove live containers.
fn parse_container_ids(body: &[u8]) -> Result<HashSet<String>> {
    #[derive(serde::Deserialize)]
    struct Container {
        #[serde(rename = "Id")]
        id: String,
    }

    let containers: Vec<Container> = serde_json::from_slice(body)
        .map_err(|e| DockerError::Server(format!("parse /containers/json body: {e}")))?;
    if containers.iter().any(|container| container.id.is_empty()) {
        return Err(DockerError::Server(
            "parse /containers/json body: empty container Id".into(),
        ));
    }
    Ok(containers
        .into_iter()
        .map(|container| container.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ids_from_docker_list() {
        let body = br#"[{"Id":"aaa","Names":["/x"]},{"Id":"bbb"}]"#;
        assert_eq!(
            parse_container_ids(body).unwrap(),
            HashSet::from(["aaa".to_string(), "bbb".to_string()])
        );
        assert!(parse_container_ids(b"[]").unwrap().is_empty());
    }

    #[test]
    fn invalid_listings_cannot_become_empty_or_partial_snapshots() {
        for body in [
            "not json",
            "null",
            r#"{"message":"not a listing"}"#,
            "[{}]",
            r#"[{"Id":null}]"#,
            r#"[{"Id":123}]"#,
            r#"[{"Id":""}]"#,
            r#"[{"Id":"alive"},{}]"#,
        ] {
            assert!(parse_container_ids(body.as_bytes()).is_err(), "{body}");
        }
    }
}
