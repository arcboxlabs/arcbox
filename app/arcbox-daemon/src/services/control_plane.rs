//! Bind host endpoints before slow runtime preparation.

use std::sync::Arc;

use anyhow::{Context, Result};
use arcbox_api::SystemServiceImpl;
use tracing::info;

use crate::context::{ControlPlane, DaemonContext};
use crate::dns_service::DnsService;

/// Starts the control plane and binds the DNS socket.
///
/// Called before `init_runtime()`. gRPC comes first so a client watching
/// the setup stream sees a DNS bind failure as `FAILED` with its cause;
/// the DNS socket is bound here, before the VM boots, so an explicitly
/// requested port that is taken costs no boot, and the previous daemon
/// has already released its socket because the lease is held.
pub async fn start_control_plane(ctx: &DaemonContext) -> Result<ControlPlane> {
    let grpc = start_grpc(ctx).await?;
    let dns = DnsService::bind_requested(ctx.dns_port, ctx.profile.dns_host_port())
        .await
        .context("Failed to start DNS service")?;
    Ok(ControlPlane { grpc, dns })
}

/// Starts the gRPC server with all services.
///
/// Setup status works immediately. Machine directory reads and web snapshots
/// become available at Runtime construction; operations wait for initialization.
async fn start_grpc(ctx: &DaemonContext) -> Result<tokio::task::JoinHandle<()>> {
    let socket_path = &ctx.layout.grpc_socket;
    let listener = crate::control_plane::bind(socket_path)?;

    info!(socket = %socket_path.display(), "control plane listening (Connect + gRPC + gRPC-Web)");

    let system_service = SystemServiceImpl::new(
        Arc::clone(&ctx.setup_state),
        Arc::clone(&ctx.shared_runtime),
        Arc::clone(&ctx.early_runtime),
    );
    let app = crate::control_plane::into_app(
        crate::control_plane::connect_router(
            Arc::clone(&ctx.shared_runtime),
            Arc::clone(&ctx.early_runtime),
            system_service,
        )?,
        ctx.rpc_calls.clone(),
    );

    let shutdown = ctx.shutdown.clone();
    Ok(tokio::spawn(crate::control_plane::serve(
        listener, app, shutdown,
    )))
}
