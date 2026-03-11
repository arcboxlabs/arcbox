//! # arcbox-api
//!
//! gRPC service implementations for `ArcBox`.
//!
//! This crate hosts service implementations consumed by the `arcbox-daemon`
//! binary. It provides machine, sandbox, and migration gRPC services.

pub mod error;
pub mod grpc;
pub mod migration;

// Re-export gRPC service types from arcbox-grpc for convenience.
pub use arcbox_grpc::v1::{
    machine_service_client, machine_service_server, migration_service_client,
    migration_service_server,
};
pub use arcbox_grpc::{
    MigrationService, MigrationServiceServer, SandboxService, SandboxServiceServer,
    SandboxSnapshotService, SandboxSnapshotServiceServer,
};

pub use error::{ApiError, Result};
pub use grpc::{MachineServiceImpl, SandboxServiceImpl, SandboxSnapshotServiceImpl};
pub use migration::MigrationServiceImpl;
