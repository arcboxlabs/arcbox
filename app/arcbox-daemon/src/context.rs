//! Shared daemon state threaded through all lifecycle phases.

use std::path::PathBuf;
use std::sync::Arc;

use arcbox_api::SetupState;
use arcbox_core::Runtime;
use tokio_util::sync::CancellationToken;

/// Daemon-wide context created during startup, passed to all phases.
pub struct DaemonContext {
    pub data_dir: PathBuf,
    pub socket_path: PathBuf,
    pub grpc_socket: PathBuf,
    pub pid_file: PathBuf,
    pub runtime: Arc<Runtime>,
    pub setup_state: Arc<SetupState>,
    pub shutdown: CancellationToken,
    pub dns_domain: String,
    pub dns_port: u16,
    pub docker_integration: bool,
}

/// Handles to spawned services for drain-on-shutdown.
pub struct ServiceHandles {
    pub dns: tokio::task::JoinHandle<()>,
    pub docker: tokio::task::JoinHandle<()>,
    pub grpc: tokio::task::JoinHandle<()>,
}
