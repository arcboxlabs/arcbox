mod dns_service;

use anyhow::{Context, Result};
use arcbox_api::{
    MachineServiceImpl, SandboxServiceImpl, SandboxServiceServer, SandboxSnapshotServiceImpl,
    SandboxSnapshotServiceServer, machine_service_server::MachineServiceServer,
};
use arcbox_core::{Config, Runtime, VmLifecycleConfig};
use arcbox_docker::{DockerApiServer, DockerContextManager, ServerConfig};
use clap::Parser;
use dns_service::DnsService;
use macos_resolver::{FileResolver, to_env_prefix};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::signal;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const DNS_PREFIX: &str = "arcbox";
const DEFAULT_DNS_DOMAIN: &str = "arcbox.local";

#[derive(Debug, Parser)]
#[command(name = "arcbox-daemon")]
#[command(author, version, about, long_about = None)]
pub struct DaemonArgs {
    /// Unix socket path for Docker API (default: ~/.arcbox/run/docker.sock).
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Unix socket path for gRPC API (default: ~/.arcbox/run/arcbox.sock).
    #[arg(long)]
    pub grpc_socket: Option<PathBuf>,

    /// Data directory for ArcBox.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Custom kernel path for VM boot.
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Automatically enable Docker CLI integration.
    #[arg(long)]
    pub docker_integration: bool,

    /// Guest dockerd API vsock port.
    #[arg(long)]
    pub guest_docker_vsock_port: Option<u32>,
}

fn main() -> Result<()> {
    // Sentry must be initialized before the tokio runtime so that spawned
    // threads inherit the Hub from the main thread.
    // When SENTRY_DSN is unset, this is a no-op with zero overhead.
    let _sentry_guard = sentry::init(sentry::ClientOptions {
        dsn: std::env::var("ARCBOX_DAEMON_SENTRY_DSN")
            .or_else(|_| std::env::var("SENTRY_DSN"))
            .ok()
            .and_then(|s| s.parse().ok()),
        release: Some(env!("CARGO_PKG_VERSION").into()),
        environment: std::env::var("ARCBOX_DAEMON_SENTRY_ENVIRONMENT")
            .or_else(|_| std::env::var("SENTRY_ENVIRONMENT"))
            .ok()
            .map(Into::into),
        traces_sample_rate: 0.2,
        sample_rate: 1.0,
        attach_stacktrace: true,
        ..Default::default()
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arcbox=info,arcbox_daemon=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(sentry::integrations::tracing::layer())
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(run(DaemonArgs::parse()))
}

async fn run(args: DaemonArgs) -> Result<()> {
    info!("Starting ArcBox daemon...");

    let data_dir = resolve_data_dir(args.data_dir.as_ref());
    let run_dir = data_dir.join(arcbox_constants::paths::host::RUN);
    let log_dir = data_dir.join(arcbox_constants::paths::host::LOG);
    let data_subdir = data_dir.join(arcbox_constants::paths::host::DATA);
    std::fs::create_dir_all(&data_dir).context("Failed to create data directory")?;
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    std::fs::create_dir_all(&data_subdir).context("Failed to create persistent data directory")?;

    let pid_file = run_dir.join("daemon.pid");
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))
        .context("Failed to write daemon PID file")?;

    let socket_path = args
        .socket
        .unwrap_or_else(|| run_dir.join("docker.sock"));
    let grpc_socket = args
        .grpc_socket
        .unwrap_or_else(|| run_dir.join("arcbox.sock"));

    let mut config = Config {
        data_dir: data_dir.clone(),
        ..Default::default()
    };
    if let Some(port) = args.guest_docker_vsock_port {
        config.container.guest_docker_vsock_port = port;
    }
    let selected_guest_docker_port = config.container.guest_docker_vsock_port;

    let mut vm_lifecycle_config = VmLifecycleConfig::default();
    vm_lifecycle_config.default_vm.cpus = config.vm.cpus;
    vm_lifecycle_config.default_vm.memory_mb = config.vm.memory_mb;
    if let Some(ref kernel) = config.vm.kernel_path {
        vm_lifecycle_config.default_vm.kernel = Some(kernel.clone());
    }
    if let Some(kernel) = args.kernel {
        vm_lifecycle_config.default_vm.kernel = Some(kernel);
    }

    let runtime = Arc::new(
        Runtime::with_vm_lifecycle_config(config, vm_lifecycle_config)
            .context("Failed to create runtime")?,
    );
    runtime
        .init()
        .await
        .context("Failed to initialize runtime")?;

    info!(
        data_dir = %data_dir.display(),
        guest_docker_vsock_port = selected_guest_docker_port,
        "Runtime initialized"
    );

    // Apply custom DNS domain if configured via ARCBOX_DNS_DOMAIN.
    let dns_local_domain = dns_domain();
    if dns_local_domain != DEFAULT_DNS_DOMAIN {
        runtime.network_manager().set_dns_domain(&dns_local_domain);
    }

    // Bind DNS socket eagerly — failure aborts the daemon.
    let dns_listen_port = dns_port();
    let dns_service = DnsService::bind(Arc::clone(runtime.network_manager()), dns_listen_port)
        .await
        .context("Failed to start DNS service")?;

    // Register host.arcbox.local → gateway IP.
    let network_cfg = &runtime.config().network;
    let gateway_ip = network_cfg
        .gateway
        .as_ref()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .or_else(|| first_address_in_subnet(&network_cfg.subnet))
        .unwrap_or(Ipv4Addr::new(192, 168, 64, 1));
    runtime
        .network_manager()
        .register_dns("host", IpAddr::V4(gateway_ip));

    let shutdown = CancellationToken::new();

    let dns_shutdown = shutdown.clone();
    let mut dns_handle = tokio::spawn(async move {
        if let Err(e) = dns_service.run(dns_shutdown).await {
            tracing::error!("DNS service error: {}", e);
        }
    });

    // Recover DNS entries for containers already running in the guest VM
    // (handles daemon restart without VM restart).
    recover_dns_entries(&runtime).await;

    let docker_server = DockerApiServer::new(
        ServerConfig {
            socket_path: socket_path.clone(),
        },
        Arc::clone(&runtime),
    );

    let docker_shutdown = shutdown.clone();
    let mut docker_handle = tokio::spawn(async move {
        if let Err(e) = docker_server.run(docker_shutdown).await {
            tracing::error!("Docker API server error: {}", e);
        }
    });

    let mut grpc_handle =
        start_grpc_server(Arc::clone(&runtime), grpc_socket.clone(), shutdown.clone()).await?;

    if args.docker_integration {
        match DockerContextManager::new(socket_path.clone()) {
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

    check_resolver_installed();

    println!("ArcBox daemon started");
    println!("  Docker API: {}", socket_path.display());
    println!("  gRPC API:   {}", grpc_socket.display());
    println!("  DNS:        127.0.0.1:{}", dns_listen_port);
    println!("  Data:       {}", data_dir.display());
    println!();
    println!("Use 'arcbox docker enable' to configure Docker CLI integration.");
    println!("Press Ctrl+C to stop.");

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

    shutdown_signal().await;
    info!("Shutdown signal received, draining connections...");
    shutdown.cancel();

    if tokio::time::timeout(DRAIN_TIMEOUT, async {
        let _ = tokio::join!(&mut dns_handle, &mut docker_handle, &mut grpc_handle);
    })
    .await
    .is_err()
    {
        warn!(
            "Server drain timed out after {}s, aborting remaining tasks",
            DRAIN_TIMEOUT.as_secs()
        );
        dns_handle.abort();
        docker_handle.abort();
        grpc_handle.abort();
    }

    runtime
        .shutdown()
        .await
        .context("Failed to shutdown runtime")?;

    if args.docker_integration {
        if let Ok(ctx_manager) = DockerContextManager::new(socket_path.clone()) {
            let _ = ctx_manager.disable();
        }
    }

    for path in [&socket_path, &grpc_socket] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove socket {}: {}", path.display(), e);
            }
        }
    }

    if let Err(e) = std::fs::remove_file(&pid_file) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove PID file {}: {}", pid_file.display(), e);
        }
    }

    info!("ArcBox daemon stopped");
    Ok(())
}

fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
    data_dir.cloned().unwrap_or_else(|| {
        dirs::home_dir().map_or_else(
            || PathBuf::from("/var/lib/arcbox"),
            |home| home.join(".arcbox"),
        )
    })
}

async fn start_grpc_server(
    runtime: Arc<Runtime>,
    socket_path: PathBuf,
    shutdown: CancellationToken,
) -> Result<tokio::task::JoinHandle<()>> {
    let _ = std::fs::remove_file(&socket_path);

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create socket directory")?;
    }

    let listener = UnixListener::bind(&socket_path).context(format!(
        "Failed to bind gRPC socket: {}",
        socket_path.display()
    ))?;
    let incoming = UnixListenerStream::new(listener);

    info!(socket = %socket_path.display(), "gRPC server listening");

    let machine_service = MachineServiceImpl::new(Arc::clone(&runtime));
    let sandbox_service = SandboxServiceImpl::new(Arc::clone(&runtime));
    let sandbox_snapshot_service = SandboxSnapshotServiceImpl::new(Arc::clone(&runtime));

    let handle = tokio::spawn(async move {
        let result = Server::builder()
            .add_service(MachineServiceServer::new(machine_service))
            .add_service(SandboxServiceServer::new(sandbox_service))
            .add_service(SandboxSnapshotServiceServer::new(sandbox_snapshot_service))
            .serve_with_incoming_shutdown(incoming, shutdown.cancelled())
            .await;

        if let Err(e) = result {
            tracing::error!("gRPC server error: {}", e);
        }
    });

    Ok(handle)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Re-registers DNS entries for all running containers in the guest VM.
///
/// Called after daemon startup to handle the case where the daemon restarts
/// but the VM (and its containers) are still running.
async fn recover_dns_entries(runtime: &Arc<Runtime>) {
    use axum::http::{HeaderMap, Method};
    use bytes::Bytes;

    let resp = match arcbox_docker::proxy::proxy_to_guest(
        runtime,
        Method::GET,
        "/containers/json",
        &HeaderMap::new(),
        Bytes::new(),
    )
    .await
    {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            tracing::debug!("Container list returned status {}", resp.status());
            return;
        }
        Err(e) => {
            tracing::debug!("Failed to list containers for DNS recovery: {}", e);
            return;
        }
    };

    let body_bytes = match http_body_util::BodyExt::collect(resp.into_body()).await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::debug!("Failed to read container list body: {}", e);
            return;
        }
    };

    let containers: Vec<serde_json::Value> = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("Failed to parse container list JSON: {}", e);
            return;
        }
    };

    let mut recovered = 0u32;
    for container in &containers {
        let Some(id) = container.get("Id").and_then(|v| v.as_str()) else {
            continue;
        };

        // Inspect each running container to get name + IP.
        let inspect_path = format!("/containers/{id}/json");
        let inspect_resp = match arcbox_docker::proxy::proxy_to_guest(
            runtime,
            Method::GET,
            &inspect_path,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            _ => continue,
        };

        let inspect_body = match http_body_util::BodyExt::collect(inspect_resp.into_body()).await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => continue,
        };

        // Reuse the same parsing logic as the container start handler.
        let Some((name, ip)) = arcbox_docker::handlers::extract_container_dns_info(&inspect_body)
        else {
            continue;
        };

        runtime.register_dns(id, &name, ip).await;
        recovered += 1;
    }

    if recovered > 0 {
        info!(
            count = recovered,
            "Recovered DNS entries for running containers"
        );
    }
}

fn dns_port() -> u16 {
    let key = format!("{}_DNS_PORT", to_env_prefix(DNS_PREFIX));
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5553)
}

/// Computes the first usable host address in a CIDR subnet.
///
/// E.g. `"192.168.64.0/24"` → `192.168.64.1`. Returns `None` for /32
/// (no host addresses) or unparseable input.
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

fn dns_domain() -> String {
    let key = format!("{}_DNS_DOMAIN", to_env_prefix(DNS_PREFIX));
    std::env::var(key).unwrap_or_else(|_| DEFAULT_DNS_DOMAIN.to_string())
}

fn check_resolver_installed() {
    let resolver = FileResolver::new(DNS_PREFIX);
    let domain = dns_domain();
    if !resolver.is_registered(&domain) {
        println!("Hint: Run 'sudo arcbox dns install' to enable *.{domain} DNS resolution.");
    }
}
