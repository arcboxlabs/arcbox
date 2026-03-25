//! ArcBox daemon — orchestrates VM, networking, and API services.

mod context;
mod dns_service;
mod nfs_mount;
mod recovery;
mod self_setup;
mod services;
mod shutdown;
mod startup;

use anyhow::Result;
use arcbox_api::SetupPhase;
use clap::Parser;
use macos_resolver::FileResolver;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "arcbox-daemon")]
#[command(author, version, about, long_about = None)]
pub struct DaemonArgs {
    /// Unix socket path for Docker API (default: ~/.arcbox/run/docker.sock).
    #[arg(long)]
    pub socket: Option<std::path::PathBuf>,

    /// Unix socket path for gRPC API (default: ~/.arcbox/run/arcbox.sock).
    #[arg(long)]
    pub grpc_socket: Option<std::path::PathBuf>,

    /// Data directory for ArcBox.
    #[arg(long)]
    pub data_dir: Option<std::path::PathBuf>,

    /// Custom kernel path for VM boot.
    #[arg(long)]
    pub kernel: Option<std::path::PathBuf>,

    /// Automatically enable Docker CLI integration.
    #[arg(long)]
    pub docker_integration: bool,

    /// Guest dockerd API vsock port.
    #[arg(long)]
    pub guest_docker_vsock_port: Option<u32>,

    /// Skip mounting the guest Docker data export at ~/ArcBox.
    #[arg(long)]
    pub no_mount_nfs: bool,
}

fn main() -> Result<()> {
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

    // Phase 1: directories, config, sockets — no runtime yet.
    let ctx = startup::init_early(args).await?;

    // Start gRPC with all services. Machine/Sandbox return UNAVAILABLE
    // until runtime is ready. SystemService works immediately so clients
    // can observe DOWNLOADING_ASSETS → ASSETS_READY progression.
    let shared_runtime = ctx.shared_runtime.clone();
    let grpc = services::start_grpc(&ctx, shared_runtime).await?;

    // Phase 2: seed/download boot assets, build runtime, start VM.
    // Progress is visible to gRPC clients via WatchSetupStatus.
    startup::init_runtime(&ctx).await?;

    // Phase 3: start remaining services that require the runtime.
    let handles = services::start_services(&ctx, grpc).await?;
    recovery::run(&ctx).await;
    nfs_mount::spawn(&ctx);

    check_resolver_installed(&ctx.dns_domain);
    ctx.setup_state.set_phase(SetupPhase::Ready, "Daemon ready");

    println!("ArcBox daemon started");
    println!("  Docker API: {}", ctx.socket_path.display());
    println!("  gRPC API:   {}", ctx.grpc_socket.display());
    println!("  DNS:        127.0.0.1:{}", ctx.dns_port);
    println!("  Data:       {}", ctx.data_dir.display());
    println!();
    println!("Use 'arcbox docker enable' to configure Docker CLI integration.");
    println!("Press Ctrl+C to stop.");

    shutdown::run(ctx, handles).await
}

fn check_resolver_installed(domain: &str) {
    let resolver = FileResolver::new("arcbox");
    if !resolver.is_registered(domain) {
        println!("Hint: Run 'sudo arcbox dns install' to enable *.{domain} DNS resolution.");
    }
}
