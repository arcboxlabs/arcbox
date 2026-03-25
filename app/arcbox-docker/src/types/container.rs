//! Container-related Docker API types.

use super::{EndpointSettings, PortBinding};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Container summary (for list).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerSummary {
    /// Container ID.
    pub id: String,
    /// Container names.
    pub names: Vec<String>,
    /// Image name.
    pub image: String,
    /// Image ID.
    pub image_id: String,
    /// Command.
    pub command: String,
    /// Created timestamp.
    pub created: i64,
    /// State.
    pub state: String,
    /// Status string.
    pub status: String,
    /// Ports.
    pub ports: Vec<Port>,
    /// Labels.
    pub labels: HashMap<String, String>,
    /// Size of files written (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_rw: Option<i64>,
    /// Size of root filesystem (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_root_fs: Option<i64>,
    /// Network settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_settings: Option<SummaryNetworkSettings>,
    /// Mounts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<MountPoint>>,
}

/// Port mapping.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Port {
    /// Private port.
    pub private_port: u16,
    /// Public port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
    /// Type (tcp/udp).
    #[serde(rename = "Type")]
    pub port_type: String,
    /// IP address (optional).
    #[serde(rename = "IP", skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

/// Network settings summary.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SummaryNetworkSettings {
    /// Networks.
    pub networks: HashMap<String, EndpointSettings>,
}

/// Mount point.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MountPoint {
    /// Mount type.
    #[serde(rename = "Type")]
    pub mount_type: String,
    /// Source.
    pub source: String,
    /// Destination.
    pub destination: String,
    /// Mode.
    pub mode: String,
    /// Read-write.
    #[serde(rename = "RW")]
    pub rw: bool,
    /// Propagation.
    pub propagation: String,
}

/// Container create request.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerCreateRequest {
    /// Hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Domain name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domainname: Option<String>,
    /// User.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Attach stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stdin: Option<bool>,
    /// Attach stdout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stdout: Option<bool>,
    /// Attach stderr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stderr: Option<bool>,
    /// Exposed ports.
    #[allow(clippy::zero_sized_map_values)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposed_ports: Option<HashMap<String, HashMap<(), ()>>>,
    /// TTY allocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,
    /// Open stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_stdin: Option<bool>,
    /// Stdin once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin_once: Option<bool>,
    /// Environment variables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    /// Command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    /// Image name.
    pub image: String,
    /// Volumes.
    #[allow(clippy::zero_sized_map_values)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<HashMap<String, HashMap<(), ()>>>,
    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// Labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
    /// Stop signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// Stop timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_timeout: Option<i32>,
    /// Host config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_config: Option<HostConfig>,
    /// Networking config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networking_config: Option<NetworkingConfig>,
}

/// Host configuration.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfig {
    /// Port bindings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_bindings: Option<HashMap<String, Vec<PortBinding>>>,
    /// Binds (volume mounts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binds: Option<Vec<String>>,
    /// Auto remove container when it exits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_remove: Option<bool>,
    /// Network mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    /// Memory limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<i64>,
    /// Memory swap limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_swap: Option<i64>,
    /// CPU shares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<i64>,
    /// CPU period.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_period: Option<i64>,
    /// CPU quota.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<i64>,
    /// Restart policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,
    /// Privileged mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
    /// PID mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_mode: Option<String>,
    /// IPC mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipc_mode: Option<String>,
    /// Read-only root filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly_rootfs: Option<bool>,
    /// Extra hosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_hosts: Option<Vec<String>>,
}

/// Networking configuration.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkingConfig {
    /// Endpoints config.
    pub endpoints_config: HashMap<String, EndpointSettings>,
}

/// Restart policy.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RestartPolicy {
    /// Policy name.
    pub name: String,
    /// Maximum retry count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_retry_count: Option<i32>,
}

/// Container create response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerCreateResponse {
    /// Container ID.
    pub id: String,
    /// Warnings.
    pub warnings: Vec<String>,
}

/// Container inspect response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerInspectResponse {
    /// Container ID.
    pub id: String,
    /// Creation time.
    pub created: String,
    /// Path to command.
    pub path: String,
    /// Command arguments.
    pub args: Vec<String>,
    /// Container state.
    pub state: ContainerState,
    /// Image.
    pub image: String,
    /// Name.
    pub name: String,
    /// Restart count.
    pub restart_count: i32,
    /// Container config.
    pub config: super::ContainerConfig,
    /// Host config.
    pub host_config: HostConfig,
    /// Network settings.
    pub network_settings: NetworkSettings,
    /// Mounts.
    pub mounts: Vec<MountPoint>,
}

/// Container state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerState {
    /// Status (created, running, paused, restarting, removing, exited, dead).
    pub status: String,
    /// Running.
    pub running: bool,
    /// Paused.
    pub paused: bool,
    /// Restarting.
    pub restarting: bool,
    /// OOM killed.
    #[serde(rename = "OOMKilled")]
    pub oom_killed: bool,
    /// Dead.
    pub dead: bool,
    /// PID.
    pub pid: i32,
    /// Exit code.
    pub exit_code: i32,
    /// Error.
    pub error: String,
    /// Started at.
    pub started_at: String,
    /// Finished at.
    pub finished_at: String,
}

/// Network settings.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkSettings {
    /// Bridge.
    pub bridge: String,
    /// Gateway.
    pub gateway: String,
    /// IP address.
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    /// IP prefix length.
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: i32,
    /// MAC address.
    pub mac_address: String,
    /// Ports.
    pub ports: HashMap<String, Option<Vec<PortBinding>>>,
    /// Networks.
    pub networks: HashMap<String, EndpointSettings>,
}

/// Wait response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct WaitResponse {
    /// Exit code.
    pub status_code: i64,
    /// Error (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WaitError>,
}

/// Wait error.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct WaitError {
    /// Error message.
    pub message: String,
}
