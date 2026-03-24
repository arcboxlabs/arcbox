//! Daemon startup: directories, PID file, config, runtime initialization.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arcbox_api::{SetupPhase, SetupState};
use arcbox_core::{BootAssetProvider, Config, Runtime, VmLifecycleConfig};
use macos_resolver::to_env_prefix;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::DaemonArgs;
use crate::context::{DaemonContext, VmArgs};

const DNS_PREFIX: &str = "arcbox";
const DEFAULT_DNS_DOMAIN: &str = "arcbox.local";

/// Phase 1: directories, PID, config, sockets. No runtime yet.
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

    let pid_file = run_dir.join("daemon.pid");
    let docker_img = data_subdir.join("docker.img");
    {
        let pf = pid_file.clone();
        let rd = run_dir.clone();
        let di = docker_img.clone();
        tokio::task::spawn_blocking(move || cleanup_stale_state(&pf, &rd, &di))
            .await
            .ok();
    }
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))
        .context("Failed to write daemon PID file")?;

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
        pid_file,
        shared_runtime: Arc::new(std::sync::OnceLock::new()),
        setup_state,
        shutdown: CancellationToken::new(),
        dns_domain,
        dns_port,
        docker_integration: args.docker_integration,
        vm_args: VmArgs {
            guest_docker_vsock_port: args.guest_docker_vsock_port,
            kernel: args.kernel,
        },
    })
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
        seed_from_bundle(&data_dir);
        ensure_agent_binary(&data_dir)
    })
    .await
    .context("seed task panicked")?
    .context("seed failed")?;
    ensure_boot_assets(&ctx.data_dir, &ctx.setup_state).await?;
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

fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
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

// =============================================================================
// App bundle detection & seeding
// =============================================================================

/// Returns the `Contents/` directory if the daemon is running inside an app bundle.
///
/// Layout: `ArcBox Desktop.app/Contents/Helpers/com.arcboxlabs.desktop.daemon`
/// → returns `ArcBox Desktop.app/Contents/`
pub(crate) fn find_bundle_contents() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // exe = .../Contents/Helpers/com.arcboxlabs.desktop.daemon
    let contents = exe.parent()?.parent()?;
    if contents.join("Resources").is_dir() && contents.join("Info.plist").exists() {
        Some(contents.to_path_buf())
    } else {
        None
    }
}

/// Seeds all assets from the app bundle to `~/.arcbox/` if available.
///
/// Copies boot assets, runtime binaries, and the agent binary from the
/// bundle so the daemon can start without network on first launch.
///
/// Bundle layout:
/// ```text
/// Contents/Resources/assets/{version}/  → ~/.arcbox/boot/{version}/
/// Contents/Resources/runtime/           → ~/.arcbox/runtime/
/// Contents/Resources/bin/arcbox-agent   → ~/.arcbox/bin/arcbox-agent
/// ```
fn seed_from_bundle(data_dir: &Path) {
    let Some(contents) = find_bundle_contents() else {
        return;
    };
    tracing::info!("App bundle detected: {}", contents.display());

    // 1. Boot assets: kernel, rootfs, manifest.
    let version = arcbox_core::boot_asset_version();
    let boot_dst = data_dir.join(format!("boot/{version}"));
    let boot_src = contents.join(format!("Resources/assets/{version}"));
    if boot_src.join("manifest.json").exists() {
        seed_dir_files(
            &boot_src,
            &boot_dst,
            &["manifest.json", "kernel", "rootfs.erofs"],
            "boot assets",
        );
    }

    // 2. Runtime binaries: dockerd, containerd, runc, etc.
    let runtime_src = contents.join("Resources/runtime");
    let runtime_dst = data_dir.join("runtime");
    if runtime_src.is_dir() {
        seed_dir_recursive(&runtime_src, &runtime_dst, "runtime binaries");
    }

    // 3. Agent binary.
    let agent_src = contents.join("Resources/bin/arcbox-agent");
    let agent_dst = data_dir.join("bin/arcbox-agent");
    if agent_src.exists() && !agent_dst.exists() {
        if let Err(e) = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(agent_dst.parent().unwrap())?;
            std::fs::copy(&agent_src, &agent_dst)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&agent_dst, std::fs::Permissions::from_mode(0o755))?;
            }
            Ok(())
        })() {
            tracing::warn!("Failed to seed agent from bundle: {e}");
        } else {
            tracing::info!("Seeded arcbox-agent from bundle");
        }
    }
}

/// Copies specific files from `src` to `dst`, skipping those already present.
fn seed_dir_files(src: &Path, dst: &Path, files: &[&str], label: &str) {
    if let Err(e) = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for name in files {
            let s = src.join(name);
            let d = dst.join(name);
            if s.exists() && !d.exists() {
                std::fs::copy(&s, &d)?;
            }
        }
        Ok(())
    })() {
        tracing::warn!("Failed to seed {label} from bundle: {e}");
    } else {
        tracing::info!("Seeded {label} from bundle");
    }
}

/// Recursively copies a directory tree, skipping files already present.
fn seed_dir_recursive(src: &Path, dst: &Path, label: &str) {
    let result = (|| -> std::io::Result<u32> {
        let mut count = 0u32;
        for entry in walkdir(src)? {
            let (rel, is_dir) = entry?;
            let d = dst.join(&rel);
            if is_dir {
                std::fs::create_dir_all(&d)?;
            } else if !d.exists() {
                if let Some(parent) = d.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(src.join(&rel), &d)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(&d)?;
                    if meta.permissions().mode() & 0o111 == 0 {
                        // Source was executable, preserve it.
                        let src_meta = std::fs::metadata(src.join(&rel))?;
                        if src_meta.permissions().mode() & 0o111 != 0 {
                            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755))?;
                        }
                    }
                }
                count += 1;
            }
        }
        Ok(count)
    })();

    match result {
        Ok(0) => tracing::debug!("{label}: already seeded"),
        Ok(n) => tracing::info!("Seeded {n} {label} from bundle"),
        Err(e) => tracing::warn!("Failed to seed {label} from bundle: {e}"),
    }
}

/// Simple recursive directory walker yielding `(relative_path, is_dir)`.
fn walkdir(root: &Path) -> std::io::Result<impl Iterator<Item = std::io::Result<(PathBuf, bool)>>> {
    let mut stack = vec![PathBuf::new()];
    let root = root.to_path_buf();
    Ok(std::iter::from_fn(move || {
        while let Some(rel) = stack.pop() {
            let abs = root.join(&rel);
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if meta.is_dir() {
                match std::fs::read_dir(&abs) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if let Ok(name) = entry.file_name().into_string() {
                                stack.push(rel.join(name));
                            }
                        }
                    }
                    Err(e) => return Some(Err(e)),
                }
                if !rel.as_os_str().is_empty() {
                    return Some(Ok((rel, true)));
                }
            } else {
                return Some(Ok((rel, false)));
            }
        }
        None
    }))
}

// =============================================================================
// Boot asset provisioning
// =============================================================================

/// Downloads kernel and rootfs if not already cached. Sets the setup phase to
/// `DownloadingAssets` while the download is in progress.
async fn ensure_boot_assets(data_dir: &Path, setup_state: &Arc<SetupState>) -> Result<()> {
    let cache_dir = data_dir.join("boot");
    let provider =
        BootAssetProvider::new(cache_dir).context("Failed to create boot asset provider")?;

    if provider.is_cached() {
        tracing::debug!("Boot assets already cached");
        return Ok(());
    }

    setup_state.set_phase(SetupPhase::DownloadingAssets, "Downloading boot assets...");
    provider
        .get_assets_with_progress(Some(Box::new(|p| {
            tracing::info!(
                name = %p.name,
                current = p.current,
                total = p.total,
                "Boot asset progress: {:?}",
                p.phase
            );
        })))
        .await
        .context("Failed to download boot assets")?;

    info!("Boot assets downloaded");
    Ok(())
}

/// Copies the arcbox-agent binary from the boot asset cache to the daemon's
/// bin directory if it is not already present.
fn ensure_agent_binary(data_dir: &Path) -> Result<()> {
    let agent_dest = data_dir.join("bin/arcbox-agent");
    if agent_dest.exists() {
        return Ok(());
    }

    let version = arcbox_core::boot_asset_version();
    let agent_src = data_dir.join(format!("boot/{version}/arcbox-agent"));
    if !agent_src.exists() {
        // Agent binary is not part of the boot cache for this version.
        // runtime.init() will fail with a clear error if the agent is truly
        // missing.
        tracing::debug!(
            "Agent binary not found in boot cache at {}",
            agent_src.display()
        );
        return Ok(());
    }

    std::fs::create_dir_all(agent_dest.parent().unwrap())
        .context("Failed to create agent bin directory")?;
    std::fs::copy(&agent_src, &agent_dest).context("Failed to copy agent binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent_dest, std::fs::Permissions::from_mode(0o755))
            .context("Failed to set agent binary permissions")?;
    }

    info!("Agent binary installed from boot cache");
    Ok(())
}

// =============================================================================
// Stale state cleanup
// =============================================================================

fn cleanup_stale_state(
    pid_file: &std::path::Path,
    run_dir: &std::path::Path,
    docker_img: &std::path::Path,
) {
    // 1. Gracefully stop stale daemon — it will shut down its VM and flush
    //    disk writes before exiting.
    if let Ok(contents) = std::fs::read_to_string(pid_file) {
        if let Ok(old_pid) = contents.trim().parse::<i32>() {
            #[allow(clippy::cast_possible_wrap)]
            let current_pid = std::process::id() as i32;
            if old_pid > 0 && old_pid != current_pid && is_process_alive(old_pid) {
                if !is_arcbox_daemon(old_pid) {
                    warn!(
                        old_pid,
                        "PID from daemon.pid is alive but is not arcbox-daemon, skipping signal"
                    );
                } else {
                    terminate_stale_daemon(old_pid);
                }
            }
        }
    }

    // 2. Wait for processes that still hold docker.img open (typically
    //    orphaned Virtualization.framework XPC helpers). We do NOT SIGKILL
    //    them — that risks corrupting the guest filesystem.
    #[cfg(target_os = "macos")]
    if docker_img.exists() {
        wait_for_docker_img_holders(docker_img);
    }

    #[cfg(not(target_os = "macos"))]
    let _ = docker_img;

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

/// Send SIGTERM to a verified stale daemon and wait for it to exit.  Falls
/// back to SIGKILL after 30 s as a last resort.
fn terminate_stale_daemon(old_pid: i32) {
    warn!(old_pid, "Stale daemon still running, sending SIGTERM");
    // SAFETY: sending a signal to a verified arcbox-daemon process.
    let ret = unsafe { libc::kill(old_pid, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        warn!(old_pid, %err, "Failed to send SIGTERM to stale daemon");
        return;
    }

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
        let ret = unsafe { libc::kill(old_pid, libc::SIGKILL) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            warn!(old_pid, %err, "Failed to send SIGKILL to stale daemon");
        }
        std::thread::sleep(Duration::from_secs(1));
    } else {
        info!(old_pid, "Stale daemon exited gracefully");
    }
}

fn is_process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) is a standard POSIX existence check.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permission to signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn is_arcbox_daemon(pid: i32) -> bool {
    match libproc::proc_pid::pidpath(pid) {
        Ok(path) => path.contains("arcbox-daemon") || path.contains("arcboxlabs.desktop.daemon"),
        Err(_) => false,
    }
}

/// Wait for processes holding `docker.img` open to exit on their own.
#[cfg(target_os = "macos")]
fn wait_for_docker_img_holders(docker_img: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let pids = match libproc::processes::pids_by_path(docker_img, false, false) {
            Ok(pids) => pids
                .into_iter()
                .filter(|&p| p != std::process::id())
                .collect::<Vec<_>>(),
            Err(e) => {
                warn!(%e, "Failed to query processes holding docker.img");
                break;
            }
        };
        if pids.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            warn!(
                ?pids,
                "Processes still holding docker.img after 10s — \
                 not killing them to avoid data loss; VM startup may fail"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
