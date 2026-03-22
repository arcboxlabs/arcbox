//! gRPC service implementations.
//!
//! This module implements the gRPC services defined in arcbox-protocol.
//! All types are imported from `arcbox_protocol::v1`, and service traits
//! are from `arcbox_grpc::v1`.

mod icon;
mod machine;
mod sandbox;
mod snapshot;

pub use icon::IconServiceImpl;
pub use machine::MachineServiceImpl;
pub use sandbox::SandboxServiceImpl;
pub use snapshot::SandboxSnapshotServiceImpl;

use std::sync::{Arc, OnceLock};

use arcbox_core::Runtime;
use tonic::Status;

/// Shared handle to a runtime that may not be initialized yet.
///
/// Services are registered with tonic before the runtime exists.
/// Each RPC method calls `runtime.ready()` which returns `UNAVAILABLE`
/// while the daemon is still downloading assets / starting the VM.
pub type SharedRuntime = Arc<OnceLock<Arc<Runtime>>>;

/// Extension trait for obtaining the runtime from a deferred handle.
trait SharedRuntimeExt {
    /// Returns the runtime, or `UNAVAILABLE` if it hasn't been initialized yet.
    fn ready(&self) -> Result<&Arc<Runtime>, Status>;
}

impl SharedRuntimeExt for SharedRuntime {
    #[allow(clippy::result_large_err)]
    fn ready(&self) -> Result<&Arc<Runtime>, Status> {
        self.get()
            .ok_or_else(|| Status::unavailable("daemon is starting, runtime not ready yet"))
    }
}

/// Extension trait for extracting routing metadata from gRPC requests.
trait RequestExt {
    /// Returns the target machine name from the `x-machine` metadata header.
    fn machine_id(&self) -> Result<String, Status>;
}

impl<T> RequestExt for tonic::Request<T> {
    #[allow(clippy::result_large_err)]
    fn machine_id(&self) -> Result<String, Status> {
        self.metadata()
            .get("x-machine")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| Status::invalid_argument("missing x-machine metadata header"))
    }
}
