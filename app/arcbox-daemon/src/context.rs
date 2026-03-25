//! Shared daemon state threaded through all lifecycle phases.

use std::path::PathBuf;
use std::sync::Arc;

use arcbox_api::{SetupState, SharedRuntime};
use arcbox_core::Runtime;
use tokio_util::sync::CancellationToken;

use crate::startup::DaemonLock;

/// Daemon-wide context created during startup, passed to all phases.
pub struct DaemonContext {
    pub data_dir: PathBuf,
    pub socket_path: PathBuf,
    pub grpc_socket: PathBuf,
    /// Exclusive lock held for the daemon's lifetime. `None` until
    /// [`startup::acquire_lock`] succeeds.
    pub daemon_lock: Option<DaemonLock>,
    /// Shared with gRPC services. Empty after `init_early`, filled by `init_runtime`.
    pub shared_runtime: SharedRuntime,
    pub setup_state: Arc<SetupState>,
    pub shutdown: CancellationToken,
    pub dns_domain: String,
    pub dns_port: u16,
    pub docker_integration: bool,
    pub mount_nfs: bool,
    pub vm_args: VmArgs,
}

impl DaemonContext {
    /// Returns the runtime. Panics if called before `init_runtime`.
    pub fn runtime(&self) -> &Arc<Runtime> {
        self.shared_runtime
            .get()
            .expect("runtime not initialized — called before init_runtime?")
    }
}

/// VM-related CLI arguments, deferred until `init_runtime`.
pub struct VmArgs {
    pub guest_docker_vsock_port: Option<u32>,
    pub kernel: Option<PathBuf>,
}

/// Handles to spawned services for drain-on-shutdown.
pub struct ServiceHandles {
    pub dns: tokio::task::JoinHandle<()>,
    pub docker: tokio::task::JoinHandle<()>,
    pub grpc: tokio::task::JoinHandle<()>,
}
