//! Service startup: DNS, Docker API, gRPC servers.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use anyhow::{Context, Result};
use arcbox_api::{
    MachineServiceImpl, SandboxServiceImpl, SandboxServiceServer, SandboxSnapshotServiceImpl,
    SandboxSnapshotServiceServer, SystemServiceImpl, SystemServiceServer,
    machine_service_server::MachineServiceServer,
};
use arcbox_docker::{DockerApiServer, DockerContextManager, ServerConfig};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};

use crate::context::{DaemonContext, ServiceHandles};
use crate::dns_service::DnsService;

/// Starts all daemon services and returns their join handles for shutdown.
pub async fn start(ctx: &DaemonContext) -> Result<ServiceHandles> {
    // DNS service.
    let dns_service = DnsService::bind(Arc::clone(ctx.runtime.network_manager()), ctx.dns_port)
        .await
        .context("Failed to start DNS service")?;

    // DNS listener is bound and ready to serve queries.
    ctx.setup_state.set_dns_installed(true);

    register_host_dns(ctx);

    let dns_shutdown = ctx.shutdown.clone();
    let dns = tokio::spawn(async move {
        if let Err(e) = dns_service.run(dns_shutdown).await {
            tracing::error!("DNS service error: {}", e);
        }
    });

    // Docker API server.
    let docker_server = DockerApiServer::new(
        ServerConfig {
            socket_path: ctx.socket_path.clone(),
        },
        Arc::clone(&ctx.runtime),
    );

    let docker_shutdown = ctx.shutdown.clone();
    let docker = tokio::spawn(async move {
        if let Err(e) = docker_server.run(docker_shutdown).await {
            tracing::error!("Docker API server error: {}", e);
        }
    });

    // gRPC server.
    let grpc = start_grpc(ctx).await?;

    // Docker CLI integration (optional).
    if ctx.docker_integration {
        match DockerContextManager::new(ctx.socket_path.clone()) {
            Ok(ctx_manager) => {
                if let Err(e) = ctx_manager.enable() {
                    warn!("Failed to enable Docker integration: {}", e);
                } else {
                    info!("Docker CLI integration enabled");
                }
            }
            Err(e) => {
                warn!("Failed to create Docker context manager: {}", e);
            }
        }
    }

    Ok(ServiceHandles { dns, docker, grpc })
}

fn register_host_dns(ctx: &DaemonContext) {
    let network_cfg = &ctx.runtime.config().network;
    let gateway_ip = network_cfg
        .gateway
        .as_ref()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .or_else(|| first_address_in_subnet(&network_cfg.subnet))
        .unwrap_or(Ipv4Addr::new(10, 0, 2, 1));
    ctx.runtime
        .network_manager()
        .register_dns("host", IpAddr::V4(gateway_ip));
}

async fn start_grpc(ctx: &DaemonContext) -> Result<tokio::task::JoinHandle<()>> {
    let socket_path = &ctx.grpc_socket;
    let _ = std::fs::remove_file(socket_path);

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create socket directory")?;
    }

    let listener = UnixListener::bind(socket_path).context(format!(
        "Failed to bind gRPC socket: {}",
        socket_path.display()
    ))?;
    let incoming = UnixListenerStream::new(listener);

    info!(socket = %socket_path.display(), "gRPC server listening");

    let machine_service = MachineServiceImpl::new(Arc::clone(&ctx.runtime));
    let sandbox_service = SandboxServiceImpl::new(Arc::clone(&ctx.runtime));
    let sandbox_snapshot_service = SandboxSnapshotServiceImpl::new(Arc::clone(&ctx.runtime));
    let system_service = SystemServiceImpl::new(Arc::clone(&ctx.setup_state));

    let shutdown = ctx.shutdown.clone();
    let handle = tokio::spawn(async move {
        let result = Server::builder()
            .add_service(MachineServiceServer::new(machine_service))
            .add_service(SandboxServiceServer::new(sandbox_service))
            .add_service(SandboxSnapshotServiceServer::new(sandbox_snapshot_service))
            .add_service(SystemServiceServer::new(system_service))
            .serve_with_incoming_shutdown(incoming, shutdown.cancelled())
            .await;

        if let Err(e) = result {
            tracing::error!("gRPC server error: {}", e);
        }
    });

    Ok(handle)
}

fn first_address_in_subnet(subnet: &str) -> Option<Ipv4Addr> {
    let (ip_str, prefix_str) = subnet.split_once('/')?;
    let base: Ipv4Addr = ip_str.parse().ok()?;
    let prefix: u8 = prefix_str.parse().ok()?;
    if prefix >= 32 {
        return None;
    }
    let mask: u32 = (!0u32) << (32 - prefix);
    let network = u32::from(base) & mask;
    let first = network.checked_add(1)?;
    let broadcast = network | !mask;
    if first > broadcast {
        None
    } else {
        Some(Ipv4Addr::from(first))
    }
}
