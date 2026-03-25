//! Exec-related Docker API types.

use serde::{Deserialize, Serialize};

/// Exec create request.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExecCreateRequest {
    /// Attach stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stdin: Option<bool>,
    /// Attach stdout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stdout: Option<bool>,
    /// Attach stderr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_stderr: Option<bool>,
    /// Console size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_size: Option<Vec<u32>>,
    /// Detach keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detach_keys: Option<String>,
    /// TTY allocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,
    /// Environment variables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    /// Command to run.
    pub cmd: Vec<String>,
    /// Privileged mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
    /// User.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Exec create response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExecCreateResponse {
    /// Exec ID.
    pub id: String,
}

/// Exec start request.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExecStartRequest {
    /// Detach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detach: Option<bool>,
    /// TTY.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,
    /// Console size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_size: Option<Vec<u32>>,
}

/// Exec inspect response.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExecInspectResponse {
    /// Can remove.
    pub can_remove: bool,
    /// Container ID.
    #[serde(rename = "ContainerID")]
    pub container_id: String,
    /// Detach keys.
    pub detach_keys: String,
    /// Exit code.
    pub exit_code: i32,
    /// Exec ID.
    #[serde(rename = "ID")]
    pub id: String,
    /// Open stderr.
    pub open_stderr: bool,
    /// Open stdin.
    pub open_stdin: bool,
    /// Open stdout.
    pub open_stdout: bool,
    /// Running.
    pub running: bool,
    /// PID.
    pub pid: i32,
}
