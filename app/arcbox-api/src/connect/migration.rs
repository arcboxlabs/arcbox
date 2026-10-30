//! Migration service — host-side surface over the runtime's migration
//! manager.
//!
//! Moving here also drops a duplicated readiness helper: this module used to
//! carry its own copy of the `ready()` extension because the gRPC one was
//! `pub(super)`. It now shares [`super::ConnectRuntimeExt`].

use arcbox_connect::v1 as pb;
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use super::SharedRuntime;
use crate::error::ApiError;

use super::ConnectRuntimeExt as _;

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

#[allow(
    refining_impl_trait,
    reason = "the trait returns `impl Encodable<M>`; naming the concrete body \
              type is strictly more informative and these impls are registered on a \
              Router rather than named by callers"
)]
impl pb::MigrationService for MigrationServiceImpl {
    async fn prepare_migration(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::PrepareMigrationRequest>,
    ) -> ServiceResult<pb::PrepareMigrationResponse> {
        let cancelled = ctx
            .extensions()
            .get::<CancellationToken>()
            .cloned()
            .unwrap_or_default();
        let response = self
            .runtime
            .ready()?
            .migration_manager()
            .prepare_migration(request.to_owned_message(), cancelled)
            .await
            .map_err(|error| {
                if matches!(
                    &error,
                    arcbox_core::CoreError::Common(arcbox_error::CommonError::Io(io))
                        if io.kind() == std::io::ErrorKind::Interrupted
                ) {
                    ConnectError::unavailable(error.to_string())
                } else {
                    ConnectError::from(ApiError::from(error))
                }
            })?;

        Response::ok(response)
    }

    async fn run_migration(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::RunMigrationRequest>,
    ) -> ServiceResult<ServiceStream<pb::RunMigrationEvent>> {
        let receiver = self
            .runtime
            .ready()?
            .migration_manager()
            .run_migration(request.to_owned_message())
            .await
            .map_err(ApiError::from)?;

        fn stream_event(
            result: arcbox_core::Result<pb::RunMigrationEvent>,
        ) -> Result<pb::RunMigrationEvent, ConnectError> {
            result.map_err(|error| ConnectError::from(ApiError::from(error)))
        }
        let stream = UnboundedReceiverStream::new(receiver).map(stream_event);

        Response::ok(Box::pin(stream))
    }
}
