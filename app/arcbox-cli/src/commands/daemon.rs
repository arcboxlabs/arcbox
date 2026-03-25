//! Daemon command implementation.
//!
//! This command controls the lifecycle of the `arcbox-daemon` binary:
//! - start in background (`arcbox daemon start`)
//! - run in foreground (`arcbox daemon start -f`)
//! - stop a running daemon (`arcbox daemon stop`)
//! - inspect daemon status (`arcbox daemon status`)

use super::machine::UnixConnector;
use anyhow::{Context, Result, bail};
use arcbox_core::DEFAULT_MACHINE_NAME;
use arcbox_grpc::v1::machine_service_client::MachineServiceClient;
use arcbox_grpc::v1::system_service_client::SystemServiceClient;
use arcbox_protocol::v1::{Empty, InspectMachineRequest, setup_status};
use clap::{Args, ValueEnum};
use humantime::format_duration;
use std::ffi::OsString;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use tokio::time::{Instant, sleep, timeout};
use tonic::Request;
use tonic::transport::Endpoint;
use tracing::warn;

const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(40);
const NFS_PORT: u16 = 2049;

/// Arguments for the daemon command.
#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Daemon action.
    #[arg(value_name = "ACTION")]
    pub action: DaemonAction,

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

    /// Run in foreground (don't daemonize).
    #[arg(long, short = 'f')]
    pub foreground: bool,

    /// Automatically enable Docker CLI integration.
    #[arg(long)]
    pub docker_integration: bool,

    /// Guest dockerd API vsock port.
    #[arg(long)]
    pub guest_docker_vsock_port: Option<u32>,

    /// Skip mounting the guest Docker data export at ~/ArcBox after start.
    #[arg(long)]
    pub no_mount_nfs: bool,
}

/// Daemon actions.
#[derive(Debug, Clone, ValueEnum)]
pub enum DaemonAction {
    Start,
    Stop,
    Status,
}

/// Executes the daemon command.
pub async fn execute(args: DaemonArgs) -> Result<()> {
    match args.action {
        DaemonAction::Start if args.foreground => exec_foreground(&args),
        DaemonAction::Start => spawn_background(&args).await,
        DaemonAction::Stop => execute_stop(&args).await,
        DaemonAction::Status => execute_status(&args).await,
    }
}

fn exec_foreground(args: &DaemonArgs) -> Result<()> {
    let daemon_binary = resolve_daemon_binary()?;
    let daemon_args = build_daemon_args(args);

    #[cfg(unix)]
    {
        let err = Command::new(&daemon_binary).args(&daemon_args).exec();
        Err(anyhow::Error::from(err)).with_context(|| {
            format!(
                "Failed to exec daemon binary at {}",
                daemon_binary.display()
            )
        })
    }

    #[cfg(not(unix))]
    {
        let status = Command::new(&daemon_binary)
            .args(&daemon_args)
            .status()
            .with_context(|| {
                format!("Failed to run daemon binary at {}", daemon_binary.display())
            })?;
        if status.success() {
            Ok(())
        } else {
            bail!("Daemon exited with status {}", status);
        }
    }
}

async fn execute_stop(args: &DaemonArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir.as_ref());
    let run_dir = data_dir.join(arcbox_constants::paths::host::RUN);
    let pid_file = run_dir.join("daemon.pid");
    let grpc_socket = args
        .grpc_socket
        .clone()
        .unwrap_or_else(|| run_dir.join("arcbox.sock"));

    let Some(pid) = read_pid_file(&pid_file)? else {
        println!("Daemon is not running");
        return Ok(());
    };

    if !process_is_running(pid) {
        let _ = std::fs::remove_file(&pid_file);
        println!("Daemon is not running");
        return Ok(());
    }

    maybe_unmount_nfs();
    send_sigterm(pid)?;
    println!("Stopping ArcBox daemon (PID {pid})...");

    let timeout_window = Duration::from_secs(40);
    let deadline = Instant::now() + timeout_window;
    loop {
        let process_exited = !process_is_running(pid);
        let grpc_socket_removed = !grpc_socket.exists();
        if process_exited && grpc_socket_removed {
            println!("ArcBox daemon stopped");
            let _ = std::fs::remove_file(&pid_file);
            return Ok(());
        }

        if Instant::now() >= deadline {
            bail!(
                "ArcBox daemon (PID {pid}) did not fully stop within {}s (process_running={}, grpc_socket_present={})",
                timeout_window.as_secs(),
                !process_exited,
                !grpc_socket_removed,
            );
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn execute_status(args: &DaemonArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir.as_ref());
    let run_dir = data_dir.join(arcbox_constants::paths::host::RUN);
    let pid_file = run_dir.join("daemon.pid");
    let docker_socket = args
        .socket
        .clone()
        .unwrap_or_else(|| run_dir.join("docker.sock"));
    let grpc_socket = args
        .grpc_socket
        .clone()
        .unwrap_or_else(|| run_dir.join("arcbox.sock"));

    let mut pid = read_pid_file(&pid_file)?;
    if let Some(value) = pid {
        if !process_is_running(value) {
            let _ = std::fs::remove_file(&pid_file);
            pid = None;
        }
    }

    let running = pid.is_some();
    let status = if running { "running" } else { "stopped" };
    let uptime = if running {
        pid_file_uptime(&pid_file).map_or_else(
            || "unknown".to_string(),
            |duration| format_duration(duration).to_string(),
        )
    } else {
        "n/a".to_string()
    };
    let grpc_responsive = if running {
        grpc_daemon_is_responsive(&grpc_socket).await
    } else {
        false
    };

    println!("ArcBox daemon status: {status}");
    println!(
        "PID: {}",
        pid.map_or_else(|| "n/a".to_string(), |value| value.to_string())
    );
    println!("Docker socket: {}", docker_socket.display());
    println!("gRPC socket: {}", grpc_socket.display());
    println!("Uptime: {uptime}");
    println!(
        "gRPC responsive: {}",
        if grpc_responsive { "yes" } else { "no" }
    );
    Ok(())
}

async fn grpc_daemon_is_responsive(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }

    let endpoint =
        Endpoint::from_static("http://[::]:50051").connect_timeout(Duration::from_secs(1));
    let connect_future =
        endpoint.connect_with_connector(UnixConnector::new(socket_path.to_path_buf()));
    matches!(
        timeout(Duration::from_secs(2), connect_future).await,
        Ok(Ok(_))
    )
}

fn pid_file_uptime(pid_file: &Path) -> Option<Duration> {
    let modified_at = std::fs::metadata(pid_file).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified_at).ok()
}

async fn spawn_background(args: &DaemonArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir.as_ref());
    let run_dir = data_dir.join(arcbox_constants::paths::host::RUN);
    let log_dir = data_dir.join(arcbox_constants::paths::host::LOG);
    let pid_file = run_dir.join("daemon.pid");
    let grpc_socket = args
        .grpc_socket
        .clone()
        .unwrap_or_else(|| run_dir.join("arcbox.sock"));
    let stdout_path = log_dir.join("daemon.stdout.log");
    let stderr_path = log_dir.join("daemon.stderr.log");

    std::fs::create_dir_all(&run_dir).context("Failed to create daemon run directory")?;
    std::fs::create_dir_all(&log_dir).context("Failed to create daemon log directory")?;

    if let Some(pid) = read_pid_file(&pid_file)? {
        if process_is_running(pid) {
            bail!("Daemon already running (PID {pid})");
        }

        warn!("Removing stale daemon PID file for PID {}", pid);
        let _ = std::fs::remove_file(&pid_file);
    }

    let daemon_binary = resolve_daemon_binary()?;
    let daemon_args = build_daemon_args(args);

    let stdout_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)
        .context("Failed to open daemon stdout log file")?;
    let stderr_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .context("Failed to open daemon stderr log file")?;

    let child = Command::new(&daemon_binary)
        .args(&daemon_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .with_context(|| {
            format!(
                "Failed to launch ArcBox daemon binary at {}",
                daemon_binary.display()
            )
        })?;

    std::fs::write(&pid_file, format!("{}\n", child.id()))
        .context("Failed to write daemon PID file")?;

    println!("ArcBox daemon started (PID {})", child.id());
    println!("  PID file: {}", pid_file.display());
    println!("  Stdout:   {}", stdout_path.display());
    println!("  Stderr:   {}", stderr_path.display());

    if !args.no_mount_nfs {
        maybe_mount_nfs(&grpc_socket).await;
    }

    Ok(())
}

fn resolve_daemon_binary() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("Failed to resolve current executable")?;

    if let Some(parent) = current_exe.parent() {
        let sibling = parent.join("arcbox-daemon");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    if let Some(path) = find_in_path("arcbox-daemon") {
        return Ok(path);
    }

    bail!("Failed to locate `arcbox-daemon` next to `arcbox` or in PATH");
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|entry| entry.join(binary))
        .find(|candidate| candidate.is_file())
}

fn build_daemon_args(args: &DaemonArgs) -> Vec<OsString> {
    let mut daemon_args = Vec::new();

    if let Some(socket) = &args.socket {
        daemon_args.push(OsString::from("--socket"));
        daemon_args.push(socket.as_os_str().to_os_string());
    }
    if let Some(grpc_socket) = &args.grpc_socket {
        daemon_args.push(OsString::from("--grpc-socket"));
        daemon_args.push(grpc_socket.as_os_str().to_os_string());
    }
    if let Some(data_dir) = &args.data_dir {
        daemon_args.push(OsString::from("--data-dir"));
        daemon_args.push(data_dir.as_os_str().to_os_string());
    }
    if let Some(kernel) = &args.kernel {
        daemon_args.push(OsString::from("--kernel"));
        daemon_args.push(kernel.as_os_str().to_os_string());
    }
    if args.docker_integration {
        daemon_args.push(OsString::from("--docker-integration"));
    }
    if let Some(port) = args.guest_docker_vsock_port {
        daemon_args.push(OsString::from("--guest-docker-vsock-port"));
        daemon_args.push(OsString::from(port.to_string()));
    }
    daemon_args
}

fn resolve_nfs_mount_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| resolve_nfs_mount_path_from_home(&home))
}

fn resolve_nfs_mount_path_from_home(home: &Path) -> PathBuf {
    home.join("ArcBox")
}

async fn maybe_mount_nfs(grpc_socket: &Path) {
    if !wait_for_daemon_ready(grpc_socket).await {
        warn!(
            socket = %grpc_socket.display(),
            "daemon did not become ready in time; skipping NFS mount"
        );
        return;
    }

    let Some(mount_path) = resolve_nfs_mount_path() else {
        warn!("could not determine home directory; skipping NFS mount");
        return;
    };

    let Some(guest_ip) = fetch_default_machine_ip(grpc_socket).await else {
        warn!("default machine has no routable IP yet; skipping NFS mount");
        return;
    };

    let expected_source = format!("{guest_ip}:/");
    match current_mount_info(&mount_path) {
        Some(info) if info.source == expected_source => return,
        Some(info) => {
            warn!(
                mount_path = %mount_path.display(),
                source = %info.source,
                fstype = %info.fstype,
                "mount path already occupied; skipping NFS mount"
            );
            return;
        }
        None => {}
    }

    if let Err(e) = std::fs::create_dir_all(&mount_path) {
        warn!(path = %mount_path.display(), error = %e, "failed to create NFS mount path");
        return;
    }

    let status = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!("vers=4,port={NFS_PORT},ro,namedattr"),
            &expected_source,
        ])
        .arg(&mount_path)
        .status();

    match status {
        Ok(exit) if exit.success() => {
            println!("Mounted guest Docker data at {}", mount_path.display());
        }
        Ok(exit) => {
            warn!(
                mount_path = %mount_path.display(),
                source = %expected_source,
                exit_code = exit.code().unwrap_or(-1),
                "mount_nfs failed"
            );
        }
        Err(e) => {
            warn!(
                mount_path = %mount_path.display(),
                source = %expected_source,
                error = %e,
                "failed to execute mount_nfs"
            );
        }
    }
}

fn maybe_unmount_nfs() {
    let Some(mount_path) = resolve_nfs_mount_path() else {
        return;
    };

    let Some(info) = current_mount_info(&mount_path) else {
        return;
    };

    if info.fstype != "nfs" {
        return;
    }

    match Command::new("/sbin/umount").arg(&mount_path).status() {
        Ok(exit) if exit.success() => {
            println!("Unmounted {}", mount_path.display());
        }
        Ok(exit) => {
            warn!(
                mount_path = %mount_path.display(),
                exit_code = exit.code().unwrap_or(-1),
                "umount failed"
            );
        }
        Err(e) => {
            warn!(mount_path = %mount_path.display(), error = %e, "failed to execute umount");
        }
    }
}

async fn wait_for_daemon_ready(grpc_socket: &Path) -> bool {
    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;

    while Instant::now() < deadline {
        if let Ok(mut client) = system_client(grpc_socket.to_path_buf()).await {
            if let Ok(resp) = client.get_setup_status(Request::new(Empty {})).await {
                if resp.into_inner().phase == setup_status::Phase::Ready as i32 {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(250)).await;
    }

    false
}

async fn system_client(
    socket_path: PathBuf,
) -> Result<SystemServiceClient<tonic::transport::Channel>> {
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(UnixConnector::new(socket_path.clone()))
        .await
        .with_context(|| {
            format!(
                "Failed to connect to ArcBox gRPC daemon at {}",
                socket_path.display()
            )
        })?;

    Ok(SystemServiceClient::new(channel))
}

async fn machine_client(
    socket_path: PathBuf,
) -> Result<MachineServiceClient<tonic::transport::Channel>> {
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(UnixConnector::new(socket_path.clone()))
        .await
        .with_context(|| {
            format!(
                "Failed to connect to ArcBox gRPC daemon at {}",
                socket_path.display()
            )
        })?;

    Ok(MachineServiceClient::new(channel))
}

async fn fetch_default_machine_ip(grpc_socket: &Path) -> Option<String> {
    let mut client = machine_client(grpc_socket.to_path_buf()).await.ok()?;
    let response = client
        .inspect(Request::new(InspectMachineRequest {
            id: DEFAULT_MACHINE_NAME.to_string(),
        }))
        .await
        .ok()?;
    let info = response.into_inner();
    let network = info.network?;
    if network.ip_address.is_empty() {
        None
    } else {
        Some(network.ip_address)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfo {
    source: String,
    fstype: String,
}

fn current_mount_info(path: &Path) -> Option<MountInfo> {
    let output = Command::new("/sbin/mount").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let target = path.to_string_lossy();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((mountpoint, info)) = parse_mount_line(line) else {
            continue;
        };
        if mountpoint != target {
            continue;
        }
        return Some(info);
    }

    None
}

fn parse_mount_line(line: &str) -> Option<(&str, MountInfo)> {
    let (source, rest) = line.split_once(" on ")?;
    let (mountpoint, suffix) = rest.split_once(" (")?;
    let fstype = suffix
        .split([',', ')'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((
        mountpoint,
        MountInfo {
            source: source.to_string(),
            fstype,
        },
    ))
}

fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
    data_dir.cloned().unwrap_or_else(|| {
        dirs::home_dir().map_or_else(
            || PathBuf::from("/var/lib/arcbox"),
            |home| home.join(".arcbox"),
        )
    })
}

fn read_pid_file(pid_file: &Path) -> Result<Option<i32>> {
    if !pid_file.exists() {
        return Ok(None);
    }

    let pid_text = std::fs::read_to_string(pid_file)
        .with_context(|| format!("Failed to read daemon PID file from {}", pid_file.display()))?;

    match pid_text.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => Ok(Some(pid)),
        _ => {
            warn!("Invalid daemon PID file, removing {}", pid_file.display());
            let _ = std::fs::remove_file(pid_file);
            Ok(None)
        }
    }
}

fn process_is_running(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    // SAFETY: libc::kill is called with a validated positive PID and signal 0,
    // which performs existence/permission checks without sending a signal.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_sigterm(pid: i32) -> Result<()> {
    // SAFETY: libc::kill is called with a validated positive PID and SIGTERM
    // to request graceful daemon shutdown.
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 {
        return Ok(());
    }

    Err(std::io::Error::last_os_error())
        .with_context(|| format!("Failed to send SIGTERM to daemon process {pid}"))
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonAction, DaemonArgs, build_daemon_args, parse_mount_line, read_pid_file,
        resolve_nfs_mount_path_from_home,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn read_pid_file_returns_none_when_missing() {
        let dir = tempdir().expect("failed to create temp dir");
        let pid_file = dir.path().join("daemon.pid");
        let pid = read_pid_file(&pid_file).expect("read_pid_file should succeed");
        assert_eq!(pid, None);
    }

    #[test]
    fn read_pid_file_parses_valid_pid() {
        let dir = tempdir().expect("failed to create temp dir");
        let pid_file = dir.path().join("daemon.pid");
        fs::write(&pid_file, "12345\n").expect("failed to write pid file");

        let pid = read_pid_file(&pid_file).expect("read_pid_file should succeed");
        assert_eq!(pid, Some(12345));
        assert!(pid_file.exists());
    }

    #[test]
    fn read_pid_file_removes_invalid_content() {
        let dir = tempdir().expect("failed to create temp dir");
        let pid_file = dir.path().join("daemon.pid");
        fs::write(&pid_file, "not-a-pid").expect("failed to write pid file");

        let pid = read_pid_file(&pid_file).expect("read_pid_file should succeed");
        assert_eq!(pid, None);
        assert!(!pid_file.exists());
    }

    #[test]
    fn build_daemon_args_omits_no_mount_nfs() {
        let args = DaemonArgs {
            action: DaemonAction::Start,
            socket: Some(PathBuf::from("/tmp/docker.sock")),
            grpc_socket: None,
            data_dir: None,
            kernel: None,
            foreground: false,
            docker_integration: false,
            guest_docker_vsock_port: None,
            no_mount_nfs: true,
        };

        let daemon_args = build_daemon_args(&args);
        assert!(
            daemon_args
                .iter()
                .all(|arg| arg.to_string_lossy() != "--no-mount-nfs")
        );
    }

    #[test]
    fn resolve_nfs_mount_path_uses_arcbox_home_dir() {
        let path = resolve_nfs_mount_path_from_home(Path::new("/Users/tester"));
        assert_eq!(path, PathBuf::from("/Users/tester/ArcBox"));
    }

    #[test]
    fn current_mount_info_parses_nfs_line() {
        let line = "10.0.0.2:/ on /Users/tester/ArcBox (nfs, nodev, nosuid, mounted by tester)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, "/Users/tester/ArcBox");
        assert_eq!(info.source, "10.0.0.2:/");
        assert_eq!(info.fstype, "nfs");
    }
}
