//! # arcbox-api
//!
//! gRPC service implementations for `ArcBox`.
//!
//! This crate hosts service implementations consumed by the `arcbox-daemon`
//! binary. It provides machine and sandbox gRPC services.

pub mod error;
pub mod grpc;
pub mod system;

// Re-export gRPC service types from arcbox-grpc for convenience.
pub use arcbox_grpc::v1::{machine_service_client, machine_service_server};
pub use arcbox_grpc::{
    SandboxService, SandboxServiceServer, SandboxSnapshotService, SandboxSnapshotServiceServer,
    SystemService, SystemServiceServer,
};

pub use arcbox_protocol::v1::setup_status::Phase as SetupPhase;
pub use error::{ApiError, Result};
pub use grpc::{MachineServiceImpl, SandboxServiceImpl, SandboxSnapshotServiceImpl};
pub use system::{SetupState, SystemServiceImpl};
