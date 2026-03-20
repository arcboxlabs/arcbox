//! Daemon startup: directories, PID file, config, runtime initialization.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arcbox_api::{SetupPhase, SetupState};
use arcbox_core::{Config, Runtime, VmLifecycleConfig};
use macos_resolver::to_env_prefix;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::DaemonArgs;
use crate::context::DaemonContext;

const DNS_PREFIX: &str = "arcbox";
const DEFAULT_DNS_DOMAIN: &str = "arcbox.local";

/// Initializes the daemon: creates directories, cleans stale state,
/// builds the runtime, and returns a fully initialized [`DaemonContext`].
pub async fn init(args: DaemonArgs) -> Result<DaemonContext> {
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
    // cleanup_stale_state uses blocking std::thread::sleep loops (up to 30s
    // waiting for a stale daemon to exit). Run it off the async runtime.
    {
        let pf = pid_file.clone();
        let rd = run_dir.clone();
        tokio::task::spawn_blocking(move || cleanup_stale_state(&pf, &rd))
            .await
            .ok();
    }
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

    setup_state.set_phase(SetupPhase::AssetsReady, "Runtime initialized");
    info!(
        data_dir = %data_dir.display(),
        guest_docker_vsock_port = selected_guest_docker_port,
        "Runtime initialized"
    );

    let dns_domain = dns_domain();
    let dns_port = dns_port();

    // Apply custom DNS domain if configured.
    if dns_domain != DEFAULT_DNS_DOMAIN {
        runtime.network_manager().set_dns_domain(&dns_domain);
    }

    Ok(DaemonContext {
        data_dir,
        socket_path,
        grpc_socket,
        pid_file,
        runtime,
        setup_state,
        shutdown: CancellationToken::new(),
        dns_domain,
        dns_port,
        docker_integration: args.docker_integration,
    })
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
// Stale state cleanup
// =============================================================================

fn cleanup_stale_state(pid_file: &std::path::Path, run_dir: &std::path::Path) {
    if let Ok(contents) = std::fs::read_to_string(pid_file) {
        if let Ok(old_pid) = contents.trim().parse::<i32>() {
            #[allow(clippy::cast_possible_wrap)]
            let current_pid = std::process::id() as i32;
            if old_pid != current_pid && is_process_alive(old_pid) && is_arcbox_daemon(old_pid) {
                warn!(old_pid, "Stale daemon still running, sending SIGTERM");
                // SAFETY: sending a signal to a verified arcbox-daemon process.
                unsafe { libc::kill(old_pid, libc::SIGTERM) };

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
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permission to signal it.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

fn is_arcbox_daemon(pid: i32) -> bool {
    match libproc::proc_pid::pidpath(pid) {
        Ok(path) => path.contains("arcbox-daemon") || path.contains("arcboxlabs.desktop.daemon"),
        Err(_) => false,
    }
}

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
            if ppid == 1 && comm.contains("com.apple.Virtualization.VirtualMachine") {
                Some(pid)
            } else {
                None
            }
        })
        .collect()
}
