//! Sandbox snapshot service — checkpoint and restore.

use arcbox_connect::sandbox_v1 as pb;
use buffa_types::google::protobuf::Empty;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};

use std::sync::Arc;

use super::sandbox_route::SandboxRuntimes;
use crate::ApiError;

use arcbox_computer::cleanup as sandbox_cleanup;
use arcbox_computer::locks::SandboxOperationLocks;

/// Sandbox snapshot service implementation.
pub struct SandboxSnapshotServiceImpl {
    runtime: SandboxRuntimes,
    operations: Arc<SandboxOperationLocks>,
}

impl SandboxSnapshotServiceImpl {
    /// Creates a new sandbox snapshot service with a deferred runtime.
    #[must_use]
    pub(super) fn new(runtime: SandboxRuntimes, operations: Arc<SandboxOperationLocks>) -> Self {
        Self {
            runtime,
            operations,
        }
    }
}

#[allow(
    refining_impl_trait,
    reason = "the trait returns `impl Encodable<M>`; naming the concrete body \
              type is strictly more informative and these impls are registered on a \
              Router rather than named by callers"
)]
impl pb::SandboxSnapshotService for SandboxSnapshotServiceImpl {
    async fn checkpoint(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::CheckpointRequest>,
    ) -> ServiceResult<pb::CheckpointResponse> {
        let req = request.to_owned_message();
        let sandbox_id = req.sandbox_id.clone();
        let (runtime, machine) = self.runtime.route_for_write(&ctx, &sandbox_id)?;
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        // The RPC error otherwise reaches only the caller; the daemon log
        // must record a failed lifecycle mutation on its own (CORE-82).
        let resp = agent
            .sandbox_checkpoint(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox checkpoint failed");
            })
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn restore(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::RestoreRequest>,
    ) -> ServiceResult<pb::RestoreResponse> {
        let req = request.to_owned_message();
        let sandbox_id = req.id.clone();
        // Request validation answers before any location's capability.
        if !sandbox_id.is_empty() {
            self.runtime
                .any()?
                .sandbox_dns_name(&sandbox_id)
                .map_err(|error| ConnectError::invalid_argument(error.to_string()))?;
        }
        // A restore runs where its checkpoint was written.
        let (runtime, machine) = self.runtime.route_snapshot(&ctx, &req.snapshot_id)?;
        runtime
            .ensure_storage_writes_available(&machine)
            .map_err(ApiError::from)?;
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let resp = agent
            .sandbox_restore(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox restore failed");
            })
            .map_err(ApiError::from)?;
        // Register restored sandbox DNS (shared live-match discipline;
        // see register_live_sandbox_dns).
        sandbox_cleanup::register_live_sandbox_dns(runtime, &machine, &resp.id, &resp.ip_address)
            .await
            .map_err(ApiError::from)?;

        Response::ok(resp)
    }

    async fn list_snapshots(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ListSnapshotsRequest>,
    ) -> ServiceResult<pb::ListSnapshotsResponse> {
        let req = request.to_owned_message();
        let (runtime, machine) = if req.sandbox_id.is_empty() {
            self.runtime.default_location(&ctx)?
        } else {
            self.runtime.route(&ctx, &req.sandbox_id)?
        };
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let resp = agent
            .sandbox_list_snapshots(req)
            .await
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn delete_snapshot(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::DeleteSnapshotRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.route_snapshot(&ctx, &req.snapshot_id)?;
        runtime
            .ensure_storage_writes_available(&machine)
            .map_err(ApiError::from)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        agent
            .sandbox_delete_snapshot(req)
            .await
            .map_err(ApiError::from)?;
        Response::ok(Empty::default())
    }
}
