//! Sandbox filesystem service — data plane.

use std::sync::Arc;

use arcbox_connect::sandbox_v1 as pb;
use arcbox_connect::sandbox_v1::{FileChunk, write_file_request};
use buffa_types::google::protobuf::Empty;
use connectrpc::{
    ConnectError, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};
use tokio_stream::StreamExt as _;

use super::{SharedRuntime, stream_input};
use crate::ApiError;

use super::sandbox_resume;
use super::{ConnectRuntimeExt as _, ContextExt as _, with_keepalive};
use arcbox_computer::SandboxHost as _;
use arcbox_computer::locks::SandboxOperationLocks;

/// Filesystem service implementation.
///
/// The local daemon forwards sandbox file operations and file bytes to the
/// System VM's agent. File operations transparently resume a paused sandbox
/// (CORE-21) unless the caller set `x-arcbox-no-auto-resume`.
pub struct SandboxFilesystemServiceImpl {
    runtime: SharedRuntime,
    operations: Arc<SandboxOperationLocks>,
}

impl SandboxFilesystemServiceImpl {
    /// Creates a new filesystem service with a deferred runtime.
    #[must_use]
    pub(super) fn new(runtime: SharedRuntime, operations: Arc<SandboxOperationLocks>) -> Self {
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
impl pb::SandboxFilesystemService for SandboxFilesystemServiceImpl {
    async fn read_file(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ReadFileRequest>,
    ) -> ServiceResult<ServiceStream<FileChunk>> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;

        // Optimistic first attempt. The guest's verdict arrives as the first
        // stream frame, so peek it: a SANDBOX_PAUSED answer becomes one
        // resume + one fresh read instead of an in-stream error.
        let agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let mut rx = agent
            .sandbox_read_file(req.clone())
            .await
            .map_err(ApiError::from)?;
        let first = match rx.recv().await {
            Some(Err(error)) if sandbox_resume::is_sandbox_paused(&error) => {
                if sandbox_resume::auto_resume_opted_out(&ctx) {
                    return Err(ConnectError::from(ApiError::from(error)));
                }
                sandbox_resume::resume(
                    runtime,
                    &self.operations,
                    &machine,
                    &req.id,
                    sandbox_resume::REASON_AUTO_RESUME,
                )
                .await?;
                let agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
                rx = agent.sandbox_read_file(req).await.map_err(ApiError::from)?;
                rx.recv().await
            }
            other => other,
        };

        let stream = tokio_stream::iter(first)
            .chain(rx)
            .map(|r| r.map_err(|e| ConnectError::from(ApiError::from(e))));
        // Keepalives are empty non-final chunks, as documented in the proto.
        let stream = with_keepalive(stream, FileChunk::default);
        Response::ok(Box::pin(stream))
    }

    async fn write_file(
        &self,
        ctx: RequestContext,
        mut requests: InboundStream<pb::WriteFileRequest>,
    ) -> ServiceResult<Empty> {
        let machine = ctx.sandbox_machine_id()?;

        // The first message in the stream must carry the Open payload.
        let first = requests.next().await.ok_or_else(|| {
            ConnectError::invalid_argument("write_file: stream closed before Open message")
        })??;
        let open = match first.to_owned_message().payload {
            Some(write_file_request::Payload::Open(open)) => *open,
            _ => {
                return Err(ConnectError::invalid_argument(
                    "write_file: first message must be Open",
                ));
            }
        };

        let runtime = self.runtime.ready()?;
        // A paused sandbox must be handled before any chunk is consumed —
        // the input stream cannot be replayed for a retry.
        sandbox_resume::ensure_resumed_for_write(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &open.id,
        )
        .await?;
        let agent = runtime.writable_agent(&machine).map_err(ApiError::from)?;

        // Only a clean `done` closes the channel for commit. An input error
        // drops the write future and its connection before a terminating frame.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::try_join!(stream_input::file(runtime, &machine, requests, tx), async {
            agent
                .sandbox_write_file(open, rx)
                .await
                .map_err(|error| ConnectError::from(ApiError::from(error)))
        })?;
        Response::ok(Empty::default())
    }

    async fn stat(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::StatFileRequest>,
    ) -> ServiceResult<pb::FileStat> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;
        let stat = sandbox_resume::with_auto_resume(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &req.id,
            || {
                let req = req.clone();
                async {
                    let mut agent = runtime.agent(&machine)?;
                    agent.sandbox_stat(req).await
                }
            },
        )
        .await?;
        Response::ok(stat)
    }

    async fn list_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ListDirRequest>,
    ) -> ServiceResult<pb::ListDirResponse> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;
        let listing = sandbox_resume::with_auto_resume(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &req.id,
            || {
                let req = req.clone();
                async {
                    let mut agent = runtime.agent(&machine)?;
                    agent.sandbox_list_dir(req).await
                }
            },
        )
        .await?;
        Response::ok(listing)
    }

    async fn make_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::MakeDirRequest>,
    ) -> ServiceResult<Empty> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;
        sandbox_resume::with_auto_resume(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &req.id,
            || {
                let req = req.clone();
                async {
                    let mut agent = runtime.writable_agent(&machine)?;
                    agent.sandbox_make_dir(req).await
                }
            },
        )
        .await?;
        Response::ok(Empty::default())
    }

    async fn remove(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::RemoveEntryRequest>,
    ) -> ServiceResult<Empty> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;
        sandbox_resume::with_auto_resume(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &req.id,
            || {
                let req = req.clone();
                async {
                    let mut agent = runtime.writable_agent(&machine)?;
                    agent.sandbox_remove_entry(req).await
                }
            },
        )
        .await?;
        Response::ok(Empty::default())
    }

    async fn r#move(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::MoveEntryRequest>,
    ) -> ServiceResult<Empty> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;
        sandbox_resume::with_auto_resume(
            runtime,
            &self.operations,
            &ctx,
            &machine,
            &req.id,
            || {
                let req = req.clone();
                async {
                    let mut agent = runtime.writable_agent(&machine)?;
                    agent.sandbox_move_entry(req).await
                }
            },
        )
        .await?;
        Response::ok(Empty::default())
    }

    async fn watch_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::WatchDirRequest>,
    ) -> ServiceResult<ServiceStream<pb::WatchDirResponse>> {
        let machine = ctx.sandbox_machine_id()?;
        let req = request.to_owned_message();
        let runtime = self.runtime.ready()?;

        // Optimistic first attempt, mirroring read_file: the guest confirms
        // an established watch with an immediate keepalive frame, so peeking
        // the first item is fast — a SANDBOX_PAUSED answer becomes one
        // resume + one fresh watch instead of an in-stream error.
        let agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let mut rx = agent
            .sandbox_watch_dir(req.clone())
            .await
            .map_err(ApiError::from)?;
        let first = match rx.recv().await {
            Some(Err(error)) if sandbox_resume::is_sandbox_paused(&error) => {
                if sandbox_resume::auto_resume_opted_out(&ctx) {
                    return Err(ConnectError::from(ApiError::from(error)));
                }
                sandbox_resume::resume(
                    runtime,
                    &self.operations,
                    &machine,
                    &req.id,
                    sandbox_resume::REASON_AUTO_RESUME,
                )
                .await?;
                let agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
                rx = agent.sandbox_watch_dir(req).await.map_err(ApiError::from)?;
                rx.recv().await
            }
            other => other,
        };

        let stream = tokio_stream::iter(first)
            .chain(rx)
            .map(|r| r.map_err(|e| ConnectError::from(ApiError::from(e))));
        // The guest already interleaves its own keepalives; this adds the
        // daemon-side ones the proto promises even if that hop stalls.
        let stream = with_keepalive(stream, || pb::WatchDirResponse {
            payload: pb::KeepAlive::default().into(),
            ..Default::default()
        });
        Response::ok(Box::pin(stream))
    }
}
