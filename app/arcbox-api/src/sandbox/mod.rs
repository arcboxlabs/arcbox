//! Platform-agnostic sandbox dispatch layer.
//!
//! This module provides [`SandboxDispatcher`] which routes sandbox operations
//! to the correct backend:
//! - **Linux**: calls [`SandboxManager`] directly (Firecracker microVMs)
//! - **macOS**: proxies through the guest agent running inside the VM
//!
//! [`AppState`] bundles the dispatcher with the runtime for use as axum state.

pub mod dispatch;

use axum::http::StatusCode;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

pub use dispatch::SandboxDispatcher;

/// Application state shared across REST handlers.
pub struct AppState {
    /// Core runtime (used by machine and system handlers).
    pub runtime: Arc<arcbox_core::Runtime>,
    /// Sandbox dispatcher (used by sandbox handlers).
    pub dispatcher: SandboxDispatcher,
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Platform-agnostic sandbox error with HTTP status mapping.
#[derive(Debug)]
pub enum SandboxError {
    /// Sandbox not found.
    NotFound(String),
    /// Sandbox already exists.
    AlreadyExists(String),
    /// Invalid sandbox state for the requested operation.
    InvalidState(String),
    /// Invalid argument in request.
    InvalidArgument(String),
    /// Sandbox subsystem is not available.
    Unavailable(String),
    /// Internal error.
    Internal(String),
}

impl SandboxError {
    /// Returns the HTTP status code for this error variant.
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::AlreadyExists(_) => StatusCode::CONFLICT,
            Self::InvalidState(_) => StatusCode::CONFLICT,
            Self::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::AlreadyExists(m)
            | Self::InvalidState(m)
            | Self::InvalidArgument(m)
            | Self::Unavailable(m)
            | Self::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl From<SandboxError> for crate::rest::types::ApiError {
    fn from(err: SandboxError) -> Self {
        Self {
            status: err.status_code(),
            message: err.message().to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Domain types
// ─────────────────────────────────────────────────────────────────────────────

/// Sandbox summary returned by list.
#[derive(Serialize)]
pub struct SandboxSummaryResult {
    pub id: String,
    pub state: String,
    pub ip_address: String,
    pub labels: HashMap<String, String>,
    pub created_at: String,
}

/// Result of creating a sandbox.
pub struct CreateSandboxResult {
    pub id: String,
    pub ip_address: String,
    pub state: String,
}

/// Parameters for creating a sandbox.
pub struct CreateSandboxParams {
    pub id: Option<String>,
    pub kernel: Option<String>,
    pub rootfs: Option<String>,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub cmd: Vec<String>,
    pub env: HashMap<String, String>,
    pub labels: HashMap<String, String>,
    pub ttl_seconds: Option<u32>,
}

/// Parameters for running a command in a sandbox.
pub struct RunSandboxParams {
    pub cmd: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub tty: bool,
    pub timeout_seconds: Option<u32>,
}

/// Sandbox inspect result.
#[derive(Serialize)]
pub struct SandboxInfoResult {
    pub id: String,
    pub state: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub created_at: String,
}

/// Platform-agnostic streaming output chunk from run/exec.
pub struct RunOutputChunk {
    pub stream: String,
    pub data: Vec<u8>,
    pub exit_code: i32,
    pub done: bool,
}
