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

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(run(DaemonArgs::parse()));
    if let Err(ref e) = result {
        eprintln!("Error: {e:?}");
    }
    result
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
    cleanup_stale_state(&pid_file, &run_dir);
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))
        .context("Failed to write daemon PID file")?;

    let socket_path = args.socket.unwrap_or_else(|| run_dir.join("docker.sock"));
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
        .unwrap_or(Ipv4Addr::new(10, 0, 2, 1));
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

    // Recover networking (DNS + port forwarding) for containers already running
    // in the guest VM (handles daemon restart without VM restart).
    recover_container_networking(&runtime).await;

    // Cold-start route reconcile: if the VM is already running (daemon
    // restarted but VM survived), re-install the host route. The route may
    // have been lost or dirtied during the daemon restart.
    // Non-blocking: retries transient failures (helper not ready, bridge
    // FDB not populated) but does not gate daemon readiness.
    #[cfg(target_os = "macos")]
    {
        use arcbox_core::DEFAULT_MACHINE_NAME;
        if let Some(mac) = runtime.machine_manager().bridge_mac(DEFAULT_MACHINE_NAME) {
            drop(tokio::spawn(async move {
                if let Err(e) = arcbox_core::route_reconciler::ensure_route_with_retry(&mac).await {
                    tracing::warn!(error = %e, "failed to install container route on cold start");
                }
            }));
        }
    }

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

    // Remove container subnet route before stopping the VM.
    #[cfg(target_os = "macos")]
    {
        tokio::task::spawn_blocking(arcbox_core::route_reconciler::remove_route)
            .await
            .ok();
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

/// Re-registers DNS entries and port forwarding for all running containers.
///
/// Called after daemon startup to handle the case where the daemon restarts
/// but the VM (and its containers) are still running.
async fn recover_container_networking(runtime: &Arc<Runtime>) {
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
            tracing::debug!("Failed to list containers for networking recovery: {}", e);
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

    let mut recovered_dns = 0u32;
    let mut recovered_ports = 0u32;
    for container in &containers {
        let Some(id) = container.get("Id").and_then(|v| v.as_str()) else {
            continue;
        };

        // Inspect each running container to get name + IP + port bindings.
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

        // DNS recovery.
        if let Some((name, ip)) = arcbox_docker::handlers::extract_container_dns_info(&inspect_body)
        {
            runtime.register_dns(id, &name, ip).await;
            recovered_dns += 1;
        }

        // Port forwarding recovery.
        let bindings = arcbox_docker::proxy::parse_port_bindings(&inspect_body);
        if !bindings.is_empty() {
            let rules: Vec<_> = bindings
                .iter()
                .map(|b| {
                    (
                        b.host_ip.clone(),
                        b.host_port,
                        b.container_port,
                        b.protocol.clone(),
                    )
                })
                .collect();
            match runtime
                .start_port_forwarding_for(runtime.default_machine_name(), id, &rules)
                .await
            {
                Ok(()) => recovered_ports += 1,
                Err(e) => tracing::warn!("Failed to recover port forwarding for {id}: {e}"),
            }
        }
    }

    if recovered_dns > 0 || recovered_ports > 0 {
        info!(
            dns = recovered_dns,
            ports = recovered_ports,
            "Recovered networking for running containers"
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

/// Clean up stale state from a previous daemon run that did not shut down
/// cleanly.
///
/// Safety strategy: always prefer graceful shutdown over forced kill.
/// SIGKILL on a VM process can corrupt the guest filesystem (BTRFS on
/// docker.img), so we give the old daemon time to shut down its VM
/// properly and only escalate as a last resort.
fn cleanup_stale_state(pid_file: &std::path::Path, run_dir: &std::path::Path) {
    // 1. Gracefully stop stale daemon — it will shut down its VM and flush
    //    disk writes before exiting.
    //    Guard against PID reuse: verify the process name before signalling.
    if let Ok(contents) = std::fs::read_to_string(pid_file) {
        if let Ok(old_pid) = contents.trim().parse::<i32>() {
            #[allow(clippy::cast_possible_wrap)]
            let current_pid = std::process::id() as i32;
            if old_pid != current_pid && is_process_alive(old_pid) && is_arcbox_daemon(old_pid) {
                warn!(old_pid, "Stale daemon still running, sending SIGTERM");
                // SAFETY: sending a signal to a verified arcbox-daemon process.
                unsafe { libc::kill(old_pid, libc::SIGTERM) };

                // Wait up to 30s for graceful shutdown (VM stop + disk flush).
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while std::time::Instant::now() < deadline && is_process_alive(old_pid) {
                    std::thread::sleep(Duration::from_millis(500));
                }

                if is_process_alive(old_pid) {
                    warn!(
                        old_pid,
                        "Stale daemon did not exit after 30s, sending SIGKILL"
                    );
                    // SAFETY: last resort — the old daemon is unresponsive.
                    unsafe { libc::kill(old_pid, libc::SIGKILL) };
                    std::thread::sleep(Duration::from_secs(1));
                } else {
                    info!(old_pid, "Stale daemon exited gracefully");
                }
            }
        }
    }

    // 2. Wait for orphaned Virtualization.framework XPC processes that were
    //    children of *our* stale daemon. We scope by checking the parent PID
    //    (should be 1/launchd if the parent daemon already exited) and only
    //    wait for processes whose PPID is 1 (true orphans).
    //    We do NOT SIGKILL them — that risks corrupting the guest filesystem.
    #[cfg(target_os = "macos")]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let pids = find_orphaned_vm_pids();
            if pids.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                warn!(
                    "Orphaned Virtualization.framework processes still alive after 10s: {:?}. \
                     Not killing them to avoid data loss — VM startup may fail.",
                    pids
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // 3. Remove stale sockets so bind() doesn't fail.
    for name in ["docker.sock", "arcbox.sock"] {
        let path = run_dir.join(name);
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("Failed to remove stale socket {}: {}", path.display(), e);
            }
        }
    }
}

fn is_process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) is a standard POSIX existence check.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Verify that the given PID is actually an arcbox-daemon process, not an
/// unrelated process that reused the PID after the old daemon exited.
///
/// Uses `libproc::pidpath` to get the full executable path directly from the
/// kernel, avoiding `ps` text parsing and its inherent race conditions.
fn is_arcbox_daemon(pid: i32) -> bool {
    match libproc::proc_pid::pidpath(pid) {
        Ok(path) => path.contains("arcbox-daemon") || path.contains("arcboxlabs.desktop.daemon"),
        Err(_) => false,
    }
}

/// Find Virtualization.framework XPC processes that are true orphans (PPID=1),
/// i.e. their parent daemon has already exited. This avoids interfering with
/// VM processes owned by other apps or a still-running daemon.
#[cfg(target_os = "macos")]
fn find_orphaned_vm_pids() -> Vec<i32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-eo", "pid,ppid,comm"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 3 {
                return None;
            }
            let pid: i32 = fields[0].parse().ok()?;
            let ppid: i32 = fields[1].parse().ok()?;
            let comm = fields[2..].join(" ");
            // Only match orphaned (PPID=1) Virtualization.framework processes.
            if ppid == 1 && comm.contains("com.apple.Virtualization.VirtualMachine") {
                Some(pid)
            } else {
                None
            }
        })
        .collect()
}

fn check_resolver_installed() {
    let resolver = FileResolver::new(DNS_PREFIX);
    let domain = dns_domain();
    if !resolver.is_registered(&domain) {
        println!("Hint: Run 'sudo arcbox dns install' to enable *.{domain} DNS resolution.");
    }
}
