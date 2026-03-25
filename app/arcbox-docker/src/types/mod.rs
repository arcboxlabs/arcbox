//! Docker API types.
//!
//! Types defined according to Docker Engine API v1.43 specification.
//! See: <https://docs.docker.com/engine/api/v1.43>

mod container;
mod exec;
mod image;
mod network;
mod volume;

pub use container::*;
pub use exec::*;
pub use image::*;
pub use network::*;
pub use volume::*;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Endpoint settings (shared by container and network types).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EndpointSettings {
    /// Network ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_id: Option<String>,
    /// Endpoint ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    /// Gateway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// IP address.
    #[serde(rename = "IPAddress", skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    /// IP prefix length.
    #[serde(rename = "IPPrefixLen", skip_serializing_if = "Option::is_none")]
    pub ip_prefix_len: Option<i32>,
    /// MAC address.
    #[serde(rename = "MacAddress", skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
}

/// Container config (shared by container inspect and image inspect).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    /// Hostname.
    pub hostname: String,
    /// User.
    pub user: String,
    /// Environment.
    pub env: Vec<String>,
    /// Command.
    pub cmd: Vec<String>,
    /// Image.
    pub image: String,
    /// Working directory.
    pub working_dir: String,
    /// Entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// Labels.
    pub labels: HashMap<String, String>,
    /// TTY.
    pub tty: bool,
    /// Open stdin.
    pub open_stdin: bool,
}

/// Port binding.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PortBinding {
    /// Host IP.
    #[serde(rename = "HostIp", skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    /// Host port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_port: Option<String>,
}

/// Prune response (shared by container and volume prune).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PruneResponse {
    /// Containers deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers_deleted: Option<Vec<String>>,
    /// Space reclaimed.
    pub space_reclaimed: u64,
}
