//! Migration gRPC service implementation.
//!
//! This module exposes the host-side migration service surface through the
//! runtime migration manager.

use arcbox_core::{CoreError, Runtime};
use arcbox_error::CommonError;
use arcbox_grpc::v1::migration_service_server;
use arcbox_protocol::v1::{
    PrepareMigrationRequest, PrepareMigrationResponse, RunMigrationEvent, RunMigrationRequest,
};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status};

fn core_to_status(error: &CoreError) -> Status {
    match error {
        CoreError::Common(CommonError::Config(message)) => {
            Status::invalid_argument(message.clone())
        }
        CoreError::Common(CommonError::NotFound(message)) => Status::not_found(message.clone()),
        CoreError::Common(CommonError::AlreadyExists(message)) => {
            Status::already_exists(message.clone())
        }
        CoreError::Common(CommonError::InvalidState(message)) => {
            Status::failed_precondition(message.clone())
        }
        CoreError::Common(CommonError::Timeout(message)) => {
            Status::deadline_exceeded(message.clone())
        }
        CoreError::Common(CommonError::PermissionDenied(message)) => {
            Status::permission_denied(message.clone())
        }
        CoreError::Common(CommonError::Io(io_error)) => Status::internal(io_error.to_string()),
        CoreError::Common(CommonError::Internal(message))
        | CoreError::Vm(message)
        | CoreError::Machine(message) => Status::internal(message.clone()),
        CoreError::Fs(error) => Status::internal(error.to_string()),
        CoreError::Net(error) => Status::internal(error.to_string()),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "tonic streaming APIs require Result<T, Status> items"
)]
fn migration_stream_status(
    result: arcbox_core::Result<RunMigrationEvent>,
) -> Result<RunMigrationEvent, Status> {
    result.map_err(|error| core_to_status(&error))
}

/// Host-side migration service implementation.
pub struct MigrationServiceImpl {
    runtime: Arc<Runtime>,
}

impl MigrationServiceImpl {
    /// Creates a new migration service.
    #[must_use]
    pub const fn new(runtime: Arc<Runtime>) -> Self {
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
            .migration_manager()
            .prepare_migration(request.into_inner())
            .await
            .map_err(|error| core_to_status(&error))?;

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
            .migration_manager()
            .run_migration(request.into_inner())
            .await
            .map_err(|error| core_to_status(&error))?;

        let stream = UnboundedReceiverStream::new(receiver).map(migration_stream_status);

        Ok(Response::new(Box::pin(stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::core_to_status;
    use arcbox_core::CoreError;
    use tonic::Code;

    #[test]
    fn invalid_state_maps_to_failed_precondition() {
        let status = core_to_status(&CoreError::invalid_state("migration manager unavailable"));

        assert_eq!(status.code(), Code::FailedPrecondition);
    }

    #[test]
    fn not_found_maps_to_not_found() {
        let status = core_to_status(&CoreError::not_found("missing plan"));

        assert_eq!(status.code(), Code::NotFound);
    }
}
