//! Daemon startup: lock acquisition, config, runtime initialization.

mod assets;
mod cleanup;
mod lock;

pub(crate) use assets::find_bundle_contents;
pub use lock::DaemonLock;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arcbox_api::{SetupPhase, SetupState};
use arcbox_core::{Config, Runtime, VmLifecycleConfig};
use macos_resolver::to_env_prefix;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::DaemonArgs;
use crate::context::{DaemonContext, VmArgs};

const DNS_PREFIX: &str = "arcbox";
const DEFAULT_DNS_DOMAIN: &str = "arcbox.local";

/// Phase 1: directories, config, sockets. No runtime, no lock yet.
///
/// Returns a context sufficient to start the gRPC SystemService so
/// clients can observe the full startup progression.
pub async fn init_early(args: DaemonArgs) -> Result<DaemonContext> {
    let setup_state = Arc::new(SetupState::new());

    let data_dir = resolve_data_dir(args.data_dir.as_ref());
    let run_dir = data_dir.join(arcbox_constants::paths::host::RUN);
    let log_dir = data_dir.join(arcbox_constants::paths::host::LOG);
    let data_subdir = data_dir.join(arcbox_constants::paths::host::DATA);
    std::fs::create_dir_all(&data_dir).context("Failed to create data directory")?;
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    std::fs::create_dir_all(&data_subdir).context("Failed to create persistent data directory")?;

    let socket_path = args.socket.unwrap_or_else(|| run_dir.join("docker.sock"));
    let grpc_socket = args
        .grpc_socket
        .unwrap_or_else(|| run_dir.join("arcbox.sock"));

    let dns_domain = dns_domain();
    let dns_port = dns_port();

    Ok(DaemonContext {
        data_dir,
        socket_path,
        grpc_socket,
        daemon_lock: None,
        shared_runtime: Arc::new(std::sync::OnceLock::new()),
        setup_state,
        shutdown: CancellationToken::new(),
        dns_domain,
        dns_port,
        mount_nfs: !args.no_mount_nfs,
        docker_integration: args.docker_integration,
        vm_args: VmArgs {
            guest_docker_vsock_port: args.guest_docker_vsock_port,
            kernel: args.kernel,
        },
    })
}

/// Acquire the daemon lock, terminating any stale daemon.
///
/// Fast when no stale daemon exists. If a stale daemon holds the lock,
/// sends SIGTERM (with 30 s grace period and SIGKILL fallback), then
/// blocks on `flock(LOCK_EX)` until the lock is released. Must run
/// before `start_grpc` because the old daemon may still be listening
/// on the same socket paths.
pub async fn acquire_lock(ctx: &mut DaemonContext) -> Result<()> {
    let run_dir = ctx.data_dir.join(arcbox_constants::paths::host::RUN);
    let lock = tokio::task::spawn_blocking(move || DaemonLock::acquire(&run_dir))
        .await
        .context("lock task panicked")?
        .context("failed to acquire daemon lock")?;
    ctx.daemon_lock = Some(lock);
    Ok(())
}

/// Wait for residual resource holders (e.g. docker.img) to release.
///
/// Must complete before [`init_runtime`] — on macOS, orphaned
/// Virtualization.framework XPC helpers may still hold the disk image.
/// Reports the `CleaningUp` phase so gRPC clients can show progress.
///
/// On non-macOS this is a no-op (no XPC helpers).
#[cfg(target_os = "macos")]
pub async fn wait_for_resources(ctx: &DaemonContext) -> Result<()> {
    let data_subdir = ctx.data_dir.join(arcbox_constants::paths::host::DATA);
    let docker_img = data_subdir.join("docker.img");

    if !docker_img.exists() {
        return Ok(());
    }

    ctx.setup_state
        .set_phase(SetupPhase::CleaningUp, "Waiting for resource release…");

    tokio::task::spawn_blocking(move || cleanup::wait_for_docker_img_holders(&docker_img))
        .await
        .context("resource wait task panicked")?;

    ctx.setup_state.set_phase(
        SetupPhase::Initializing,
        "Finished waiting for resource release (see logs for details)",
    );
    Ok(())
}

/// No-op on non-macOS — no Virtualization.framework XPC helpers.
#[cfg(not(target_os = "macos"))]
pub async fn wait_for_resources(_ctx: &DaemonContext) -> Result<()> {
    Ok(())
}

/// Phase 2: seed/download boot assets, build runtime, start VM.
///
/// Called after gRPC SystemService is already listening so clients
/// can observe DOWNLOADING_ASSETS → ASSETS_READY progression.
pub async fn init_runtime(ctx: &mut DaemonContext) -> Result<()> {
    let mut config = Config {
        data_dir: ctx.data_dir.clone(),
        ..Default::default()
    };
    if let Some(port) = ctx.vm_args.guest_docker_vsock_port {
        config.container.guest_docker_vsock_port = port;
    }
    let selected_guest_docker_port = config.container.guest_docker_vsock_port;

    let mut vm_lifecycle_config = VmLifecycleConfig::default();
    vm_lifecycle_config.default_vm.cpus = config.vm.cpus;
    vm_lifecycle_config.default_vm.memory_mb = config.vm.memory_mb;
    if let Some(ref kernel) = config.vm.kernel_path {
        vm_lifecycle_config.default_vm.kernel = Some(kernel.clone());
    }
    if let Some(ref kernel) = ctx.vm_args.kernel {
        vm_lifecycle_config.default_vm.kernel = Some(kernel.clone());
    }

    // Seed boot assets from app bundle if available, otherwise download.
    // Run on a blocking thread to avoid stalling the async runtime during
    // large file copies (kernel + rootfs + runtime binaries).
    let data_dir = ctx.data_dir.clone();
    tokio::task::spawn_blocking(move || {
        assets::seed_from_bundle(&data_dir);
        assets::ensure_agent_binary(&data_dir)
    })
    .await
    .context("seed task panicked")?
    .context("seed failed")?;
    assets::ensure_boot_assets(&ctx.data_dir, &ctx.setup_state).await?;
    ctx.setup_state
        .set_phase(SetupPhase::AssetsReady, "Boot assets ready");

    let runtime = Arc::new(
        Runtime::with_vm_lifecycle_config(config, vm_lifecycle_config)
            .context("Failed to create runtime")?,
    );
    runtime
        .init()
        .await
        .context("Failed to initialize runtime")?;
    info!(
        data_dir = %ctx.data_dir.display(),
        guest_docker_vsock_port = selected_guest_docker_port,
        "Runtime initialized"
    );

    if ctx.dns_domain != DEFAULT_DNS_DOMAIN {
        runtime.network_manager().set_dns_domain(&ctx.dns_domain);
    }

    ctx.shared_runtime
        .set(runtime)
        .map_err(|_| anyhow::anyhow!("init_runtime called twice"))?;
    Ok(())
}

pub fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
    data_dir.cloned().unwrap_or_else(|| {
        dirs::home_dir().map_or_else(
            || PathBuf::from("/var/lib/arcbox"),
            |home| home.join(".arcbox"),
        )
    })
}

fn dns_port() -> u16 {
    let key = format!("{}_DNS_PORT", to_env_prefix(DNS_PREFIX));
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5553)
}

fn dns_domain() -> String {
    let key = format!("{}_DNS_DOMAIN", to_env_prefix(DNS_PREFIX));
    std::env::var(key).unwrap_or_else(|_| DEFAULT_DNS_DOMAIN.to_string())
}
