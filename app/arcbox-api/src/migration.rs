//! Migration gRPC service implementation.
//!
//! This module exposes the host-side migration service surface through the
//! runtime migration manager.

use arcbox_grpc::v1::migration_service_server;
use arcbox_protocol::v1::{
    PrepareMigrationRequest, PrepareMigrationResponse, RunMigrationEvent, RunMigrationRequest,
};
use std::pin::Pin;
use tokio_stream::Stream;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status};

use crate::error::ApiError;
use crate::grpc::SharedRuntime;

/// Extension trait for obtaining the runtime from a deferred handle
/// (duplicates the one in `grpc::mod` because that trait is `pub(super)`).
trait SharedRuntimeReady {
    #[allow(clippy::result_large_err)]
    fn ready(&self) -> Result<&std::sync::Arc<arcbox_core::Runtime>, Status>;
}

impl SharedRuntimeReady for SharedRuntime {
    #[allow(clippy::result_large_err)]
    fn ready(&self) -> Result<&std::sync::Arc<arcbox_core::Runtime>, Status> {
        self.get()
            .ok_or_else(|| Status::unavailable("daemon is starting, runtime not ready yet"))
    }
}

/// Host-side migration service implementation.
pub struct MigrationServiceImpl {
    runtime: SharedRuntime,
}

impl MigrationServiceImpl {
    /// Creates a new migration service with a deferred runtime.
    #[must_use]
    pub fn new(runtime: SharedRuntime) -> Self {
        Self { runtime }
    }
}

#[tonic::async_trait]
impl migration_service_server::MigrationService for MigrationServiceImpl {
    async fn prepare_migration(
        &self,
        request: Request<PrepareMigrationRequest>,
    ) -> Result<Response<PrepareMigrationResponse>, Status> {
        let response = self
            .runtime
            .ready()?
            .migration_manager()
            .prepare_migration(request.into_inner())
            .await
            .map_err(ApiError::from)?;

        Ok(Response::new(response))
    }

    type RunMigrationStream =
        Pin<Box<dyn Stream<Item = Result<RunMigrationEvent, Status>> + Send + 'static>>;

    async fn run_migration(
        &self,
        request: Request<RunMigrationRequest>,
    ) -> Result<Response<Self::RunMigrationStream>, Status> {
        let receiver = self
            .runtime
            .ready()?
            .migration_manager()
            .run_migration(request.into_inner())
            .await
            .map_err(ApiError::from)?;

        let stream = UnboundedReceiverStream::new(receiver)
            .map(|r| r.map_err(|e| Status::internal(e.to_string())));

        Ok(Response::new(Box::pin(stream)))
    }
}
