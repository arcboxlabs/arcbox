//! Linux implementation of the Guest Agent.
//!
//! Listens on vsock and dispatches RPC requests on the Linux guest.
//! Non-Linux platforms use the stub in `super::stub`.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::AGENT_PORT;
use super::ensure_runtime;
use crate::rpc::{
    AGENT_VERSION, ErrorResponse, RpcRequest, RpcResponse, parse_request, read_message,
    write_response,
};
use arcbox_constants::paths::{
    ARCBOX_RUNTIME_BIN_DIR, CNI_DATA_MOUNT_POINT, CONTAINERD_SOCKET, DOCKER_API_UNIX_SOCKET,
    DOCKER_DATA_MOUNT_POINT, K3S_CNI_BIN_DIR, K3S_CNI_CONF_DIR, K3S_DATA_MOUNT_POINT,
    K3S_KUBECONFIG_PATH, KUBELET_DATA_MOUNT_POINT,
};
use arcbox_constants::ports::KUBERNETES_API_GUEST_PORT;
use arcbox_constants::status::{SERVICE_ERROR, SERVICE_NOT_READY, SERVICE_READY};

use arcbox_protocol::agent::{
    KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesKubeconfigRequest,
    KubernetesKubeconfigResponse, KubernetesStartRequest, KubernetesStartResponse,
    KubernetesStatusRequest, KubernetesStatusResponse, KubernetesStopRequest,
    KubernetesStopResponse, PingResponse, RuntimeEnsureRequest, RuntimeEnsureResponse,
    RuntimeStatusRequest, RuntimeStatusResponse,
};

mod btrfs;
mod cmdline;
mod probe;
mod proxy;
mod sandbox;
mod system_info;
mod vsock;

use btrfs::ensure_data_mount;
use cmdline::docker_api_vsock_port;
use probe::{probe_docker_api_ready, probe_first_ready_socket, probe_tcp, probe_unix_socket};
use proxy::{run_docker_api_proxy, run_kubernetes_api_proxy};
use sandbox::{handle_sandbox_message, sandbox_service};
use system_info::handle_get_system_info;
use vsock::{bind_vsock_listener_with_retry, is_peer_closed_error};

/// Containerd socket candidates (primary + legacy fallback).
const CONTAINERD_SOCKET_CANDIDATES: [&str; 2] =
    [CONTAINERD_SOCKET, "/var/run/containerd/containerd.sock"];
const K3S_PID_FILE: &str = "/run/arcbox/k3s.pid";
const K3S_NAMESPACE: &str = "k8s.io";
const KUBERNETES_HOST_ENDPOINT: &str = "https://127.0.0.1:16443";

/// Result from handling a request.
enum RequestResult {
    /// Single response.
    Single(RpcResponse),
}

/// The Guest Agent.
///
/// Listens on vsock and handles RPC requests from the host.
pub struct Agent;

impl Agent {
    /// Creates a new agent.
    pub fn new() -> Self {
        Self
    }

    /// Runs the agent, listening on vsock.
    pub async fn run(&self) -> Result<()> {
        // Mount standard VirtioFS shares if not already mounted.
        crate::mount::mount_standard_shares();

        // Eagerly initialise the sandbox service so its first-time
        // NetworkManager setup (which requires root) happens at startup
        // rather than on the first sandbox request.
        let _ = sandbox_service();

        // Start guest-side Docker API proxy (vsock -> unix socket).
        tokio::spawn(async {
            if let Err(e) = run_docker_api_proxy().await {
                tracing::warn!("Docker API proxy exited: {}", e);
            }
        });

        // Start guest-side Kubernetes API proxy (vsock -> localhost:6443).
        tokio::spawn(async {
            if let Err(e) = run_kubernetes_api_proxy().await {
                tracing::warn!("Kubernetes API proxy exited: {}", e);
            }
        });

        let mut listener = bind_vsock_listener_with_retry(AGENT_PORT, "agent rpc listener").await?;

        tracing::info!("Agent listening on vsock port {}", AGENT_PORT);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::info!("Accepted connection from {:?}", peer_addr);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream).await {
                            // A routine daemon-side teardown (host closes the
                            // socketpair while the agent is writing a response)
                            // surfaces as BrokenPipe / ConnectionReset /
                            // UnexpectedEof. Log at warn — the daemon will
                            // reopen on its next poll iteration.
                            if is_peer_closed_error(&e) {
                                tracing::warn!("Connection closed by peer: {}", e);
                            } else {
                                tracing::error!("Connection error: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Accept error: {}", e);
                }
            }
        }
    }
}

impl Default for Agent {
    fn default() -> Self {
        Self::new()
    }
}

/// Handles a single vsock connection.
///
/// Reads RPC requests, processes them, and writes responses.
async fn handle_connection<S>(mut stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Read the next request (V2 wire format with trace_id).
        let (msg_type, trace_id, payload) = match read_message(&mut stream).await {
            Ok(msg) => msg,
            Err(e) => {
                // Check if it's an EOF (clean disconnect)
                if e.to_string().contains("failed to read message header") {
                    tracing::debug!("Client disconnected");
                    return Ok(());
                }
                return Err(e);
            }
        };

        tracing::info!(
            trace_id = %trace_id,
            "Received message type {:?}, payload_len={}",
            msg_type,
            payload.len()
        );

        // Sandbox requests are handled separately — they bypass the normal
        // RPC request/response cycle because streaming operations hold the
        // connection open and write multiple frames.
        if msg_type.is_sandbox_request() {
            if let Err(e) = handle_sandbox_message(&mut stream, msg_type, &trace_id, &payload).await
            {
                tracing::warn!(trace_id = %trace_id, error = %e, "sandbox handler error");
            }
            continue;
        }

        // Parse and handle the request.
        let result = match parse_request(msg_type, &payload) {
            Ok(request) => handle_request(request).await,
            Err(e) => {
                tracing::warn!(trace_id = %trace_id, "Failed to parse request: {}", e);
                RequestResult::Single(RpcResponse::Error(ErrorResponse::new(
                    400,
                    format!("invalid request: {}", e),
                )))
            }
        };

        // Handle the result, echoing back the trace_id in responses.
        match result {
            RequestResult::Single(response) => {
                // Write single response
                write_response(&mut stream, &response, &trace_id).await?;
            }
        }
    }
}

/// Handles a single RPC request.
async fn handle_request(request: RpcRequest) -> RequestResult {
    match request {
        RpcRequest::Ping(req) => RequestResult::Single(handle_ping(req)),
        RpcRequest::GetSystemInfo => RequestResult::Single(handle_get_system_info().await),
        RpcRequest::EnsureRuntime(req) => RequestResult::Single(handle_ensure_runtime(req).await),
        RpcRequest::RuntimeStatus(req) => RequestResult::Single(handle_runtime_status(req).await),
        RpcRequest::StartKubernetes(req) => {
            RequestResult::Single(handle_start_kubernetes(req).await)
        }
        RpcRequest::StopKubernetes(req) => RequestResult::Single(handle_stop_kubernetes(req).await),
        RpcRequest::DeleteKubernetes(req) => {
            RequestResult::Single(handle_delete_kubernetes(req).await)
        }
        RpcRequest::KubernetesStatus(req) => {
            RequestResult::Single(handle_kubernetes_status(req).await)
        }
        RpcRequest::KubernetesKubeconfig(req) => {
            RequestResult::Single(handle_kubernetes_kubeconfig(req).await)
        }
        RpcRequest::Shutdown(req) => RequestResult::Single(handle_shutdown(req)),
        RpcRequest::MmapReadFile(req) => RequestResult::Single(handle_mmap_read_file(req)),
    }
}

/// Handles a Shutdown request.
///
/// Responds immediately so the host receives the ack, then spawns the
/// shutdown sequence in a background OS thread (not a tokio task) because
/// [`crate::shutdown::poweroff`] calls blocking libc functions and never
/// returns.
fn handle_shutdown(req: arcbox_protocol::agent::ShutdownRequest) -> RpcResponse {
    let grace = if req.timeout_seconds == 0 {
        Duration::from_secs(u64::from(
            arcbox_constants::timeouts::GUEST_SHUTDOWN_GRACE_SECS,
        ))
    } else {
        Duration::from_secs(u64::from(req.timeout_seconds))
    };
    tracing::info!(grace_secs = grace.as_secs(), "Shutdown requested by host");
    std::thread::spawn(move || {
        // Brief delay so the response frame flushes over vsock.
        std::thread::sleep(Duration::from_millis(100));
        crate::shutdown::poweroff(grace);
    });
    RpcResponse::Shutdown(arcbox_protocol::agent::ShutdownResponse { accepted: true })
}

/// Sets CLOCK_REALTIME from the given timestamp (seconds since UNIX epoch).
///
/// Idempotent — safe to call more than once. Returns `true` if the clock
/// was set successfully.
fn sync_clock_from_host(timestamp_secs: i64) -> bool {
    if timestamp_secs <= 0 {
        return false;
    }
    let ts = libc::timespec {
        tv_sec: timestamp_secs,
        tv_nsec: 0,
    };
    // SAFETY: `ts` points to a valid initialized timespec for this call,
    // and CLOCK_REALTIME is a valid clock ID on Linux guests.
    let ret = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if ret != 0 {
        tracing::warn!(
            timestamp_secs,
            error = %std::io::Error::last_os_error(),
            "failed to set clock from host"
        );
        return false;
    }
    true
}

/// Handles a `MmapReadFile` request (test-only).
///
/// Opens the requested file `O_RDONLY`, calls `mmap(MAP_SHARED)`, reads
/// the mapped region into a heap buffer, then `munmap`s. When the path
/// is on a VirtioFS mount, the `mmap` call triggers `FUSE_SETUPMAPPING`
/// on the host, which exercises the DAX path end-to-end (ABX-362).
///
/// Page-alignment: `mmap` requires page-aligned offsets, so we round
/// `offset` down to the nearest page and slice the returned bytes to
/// compensate. This matches how real userspace code uses `mmap`.
fn handle_mmap_read_file(req: arcbox_protocol::agent::MmapReadFileRequest) -> RpcResponse {
    use std::os::unix::io::AsRawFd;

    const PAGE_SIZE: u64 = 4096;

    if req.length == 0 {
        return RpcResponse::MmapReadFile(arcbox_protocol::agent::MmapReadFileResponse {
            data: Vec::new(),
            bytes_read: 0,
        });
    }
    // Hard cap to keep the response message size bounded.
    if req.length > 64 * 1024 * 1024 {
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EINVAL,
            "MmapReadFile length exceeds 64 MiB cap",
        ));
    }

    let file = match std::fs::OpenOptions::new().read(true).open(&req.path) {
        Ok(f) => f,
        Err(e) => {
            return RpcResponse::Error(crate::rpc::ErrorResponse::new(
                e.raw_os_error().unwrap_or(libc::EIO),
                format!("open {}: {e}", req.path),
            ));
        }
    };

    let page_base = req.offset & !(PAGE_SIZE - 1);
    let inner_off = (req.offset - page_base) as usize;
    let total_len = inner_off + req.length as usize;
    let mapped_len = total_len.div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize;

    // SAFETY: mapped_len is > 0 and page-aligned. We pass PROT_READ and a
    // valid file fd; on success we own the mapping and must munmap it.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            #[allow(clippy::cast_possible_wrap)]
            {
                page_base as libc::off_t
            },
        )
    };
    if ptr == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            e.raw_os_error().unwrap_or(libc::EIO),
            format!("mmap {}: {e}", req.path),
        ));
    }

    // SAFETY: [ptr + inner_off .. ptr + inner_off + req.length] is within
    // the mapping we just obtained. Copy into a heap buffer so we can
    // unmap before returning.
    let data = unsafe {
        let slice =
            std::slice::from_raw_parts(ptr.cast::<u8>().add(inner_off), req.length as usize);
        slice.to_vec()
    };
    // SAFETY: ptr/mapped_len were returned by mmap above.
    unsafe {
        libc::munmap(ptr, mapped_len);
    }

    let bytes_read = data.len() as u64;
    RpcResponse::MmapReadFile(arcbox_protocol::agent::MmapReadFileResponse { data, bytes_read })
}

/// Handles a Ping request.
fn handle_ping(req: arcbox_protocol::agent::PingRequest) -> RpcResponse {
    tracing::debug!("Ping request: {:?}", req.message);
    sync_clock_from_host(req.timestamp_secs);
    RpcResponse::Ping(PingResponse {
        message: if req.message.is_empty() {
            "pong".to_string()
        } else {
            format!("pong: {}", req.message)
        },
        version: AGENT_VERSION.to_string(),
    })
}

/// Idempotent, non-blocking EnsureRuntime handler.
///
/// The first driver spawns [`do_ensure_runtime_start`] as a background
/// task and returns `STATUS_STARTING` immediately; subsequent callers see
/// the in-progress state and also return `STATUS_STARTING`. The daemon
/// polls until the state settles into `Ready`/`Failed`. See
/// [`ensure_runtime::ensure_runtime`] for the rationale.
async fn handle_ensure_runtime(req: RuntimeEnsureRequest) -> RpcResponse {
    let guard = ensure_runtime::runtime_guard();

    let response = ensure_runtime::ensure_runtime(
        guard,
        req.start_if_needed,
        || do_ensure_runtime_start(),
        || do_ensure_runtime_probe(),
    )
    .await;

    RpcResponse::RuntimeEnsure(response)
}

/// Performs the actual runtime start sequence (called only by the driver).
async fn do_ensure_runtime_start() -> RuntimeEnsureResponse {
    let mut notes = Vec::new();
    let note = try_start_bundled_runtime().await;
    if !note.is_empty() {
        notes.push(note);
    }

    // Poll until docker socket is ready (up to ~90 seconds).
    // On large data volumes with VirtIO block I/O, containerd may need
    // up to 30s to scan its content store, and dockerd may need additional
    // time to load containers from Btrfs.
    let mut status = collect_runtime_status().await;
    for _ in 0..180 {
        if status.docker_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        status = collect_runtime_status().await;
    }

    let mut message = status.detail.clone();
    if !notes.is_empty() {
        message = format!("{}; {}", notes.join("; "), status.detail);
    }

    let result_status = if status.docker_ready {
        ensure_runtime::STATUS_STARTED.to_string()
    } else {
        ensure_runtime::STATUS_FAILED.to_string()
    };

    RuntimeEnsureResponse {
        ready: status.docker_ready,
        endpoint: status.endpoint,
        message,
        status: result_status,
    }
}

/// Probes runtime status without attempting to start (for start_if_needed=false).
async fn do_ensure_runtime_probe() -> RuntimeEnsureResponse {
    let status = collect_runtime_status().await;
    RuntimeEnsureResponse {
        ready: status.docker_ready,
        endpoint: status.endpoint,
        message: status.detail,
        status: if status.docker_ready {
            ensure_runtime::STATUS_REUSED.to_string()
        } else {
            ensure_runtime::STATUS_FAILED.to_string()
        },
    }
}

async fn handle_runtime_status(_req: RuntimeStatusRequest) -> RpcResponse {
    RpcResponse::RuntimeStatus(collect_runtime_status().await)
}

fn kubernetes_control_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn read_pid_file(path: &str) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

fn write_pid_file(path: &str, pid: u32) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{pid}\n"))?;
    Ok(())
}

fn remove_pid_file(path: &str) {
    let _ = std::fs::remove_file(path);
}

fn process_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn k3s_pid() -> Option<i32> {
    let pid = read_pid_file(K3S_PID_FILE)?;
    if process_alive(pid) {
        Some(pid)
    } else {
        remove_pid_file(K3S_PID_FILE);
        None
    }
}

async fn k3s_api_ready() -> bool {
    probe_tcp(SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        KUBERNETES_API_GUEST_PORT,
    ))
    .await
}

fn runtime_path_env(runtime_bin_dir: &Path) -> String {
    let standard = "/usr/sbin:/usr/bin:/sbin:/bin";
    match std::env::var("PATH") {
        Ok(existing) if !existing.is_empty() => {
            format!("{}:{}:{}", runtime_bin_dir.display(), existing, standard)
        }
        _ => format!("{}:{}", runtime_bin_dir.display(), standard),
    }
}

fn ensure_shared_runtime_dirs(notes: &mut Vec<String>) {
    for dir in [
        "/run/containerd",
        "/var/run/docker",
        "/etc/docker",
        "/etc/containerd",
        "/run/arcbox",
        K3S_CNI_CONF_DIR,
        K3S_CNI_BIN_DIR,
    ] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            notes.push(format!("mkdir {} failed({})", dir, e));
        }
    }
}

pub(super) fn shared_containerd_config() -> String {
    format!(
        "version = 2\n[plugins.\"io.containerd.grpc.v1.cri\".cni]\n  bin_dir = \"{K3S_CNI_BIN_DIR}\"\n  conf_dir = \"{K3S_CNI_CONF_DIR}\"\n  max_conf_num = 1\n"
    )
}

async fn ensure_containerd_ready(runtime_bin_dir: &Path, notes: &mut Vec<String>) -> bool {
    if probe_first_ready_socket(&CONTAINERD_SOCKET_CANDIDATES).await {
        return true;
    }

    let containerd_config = "/etc/containerd/config.toml";
    let config_toml = shared_containerd_config();
    if let Err(e) = std::fs::write(containerd_config, config_toml) {
        notes.push(format!("write containerd config failed({})", e));
    }

    let path_env = runtime_path_env(runtime_bin_dir);
    let containerd_bin = runtime_bin_dir.join("containerd");
    let mut cmd = Command::new(&containerd_bin);
    cmd.args([
        "--config",
        containerd_config,
        "--address",
        CONTAINERD_SOCKET,
        "--state",
        "/run/containerd",
    ])
    .env("PATH", &path_env)
    .stdin(Stdio::null())
    .stdout(daemon_log_file("containerd"))
    .stderr(daemon_log_file("containerd"));

    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id().unwrap_or_default();
            tracing::info!(pid, "spawned bundled containerd");
            notes.push(format!("spawned bundled containerd (pid={})", pid));
        }
        Err(e) => {
            notes.push(format!("failed to spawn bundled containerd: {}", e));
            return false;
        }
    }

    // On large data volumes (Btrfs with many layers/snapshots), containerd
    // may need significant time to scan its content store on first boot.
    // VirtIO block I/O is slower than VZ native disk, so allow up to 30s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() < deadline {
        if probe_first_ready_socket(&CONTAINERD_SOCKET_CANDIDATES).await {
            let elapsed = start.elapsed();
            tracing::info!(
                elapsed_ms = elapsed.as_millis() as u64,
                "containerd socket poll complete containerd_ready=true"
            );
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    notes.push("containerd socket not ready after 30s".to_string());
    false
}

async fn ensure_dockerd_ready(runtime_bin_dir: &Path, notes: &mut Vec<String>) {
    if probe_unix_socket(DOCKER_API_UNIX_SOCKET).await {
        return;
    }

    let path_env = runtime_path_env(runtime_bin_dir);
    let dockerd_bin = runtime_bin_dir.join("dockerd");
    let mut cmd = Command::new(&dockerd_bin);
    cmd.arg(format!("--host=unix://{DOCKER_API_UNIX_SOCKET}"))
        .arg(format!("--containerd={CONTAINERD_SOCKET}"))
        .arg("--exec-root=/var/run/docker")
        .arg(format!("--data-root={DOCKER_DATA_MOUNT_POINT}"))
        .arg("--userland-proxy=false")
        .env("PATH", &path_env)
        .stdin(Stdio::null())
        .stdout(daemon_log_file("dockerd"))
        .stderr(daemon_log_file("dockerd"));

    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id().unwrap_or_default();
            tracing::info!(pid, "spawned bundled dockerd");
            notes.push(format!("spawned bundled dockerd (pid={})", pid));
        }
        Err(e) => {
            notes.push(format!("failed to spawn bundled dockerd: {}", e));
        }
    }
}

async fn collect_runtime_status() -> RuntimeStatusResponse {
    use arcbox_protocol::agent::ServiceStatus;

    let containerd_ready = probe_first_ready_socket(&CONTAINERD_SOCKET_CANDIDATES).await;
    // Two-level check: socket connectable (fast) + HTTP API probe (strong).
    // Use socket connectable as the ready gate so the daemon doesn't stall
    // when dockerd takes >30s to fully initialize (Loading containers on
    // large data volumes). The HTTP probe result is included in the detail.
    let docker_socket_ok = probe_unix_socket(DOCKER_API_UNIX_SOCKET).await;
    let docker_api_ok = if docker_socket_ok {
        probe_docker_api_ready(DOCKER_API_UNIX_SOCKET).await
    } else {
        false
    };
    let docker_ready = docker_socket_ok;
    let runtime_dir = PathBuf::from(ARCBOX_RUNTIME_BIN_DIR);
    let missing_runtime_binaries = missing_runtime_binaries_at(&runtime_dir);

    // Build per-service status entries.
    let mut services = Vec::new();

    // containerd status
    services.push(if containerd_ready {
        ServiceStatus {
            name: "containerd".to_string(),
            status: SERVICE_READY.to_string(),
            detail: format!(
                "socket reachable: {}",
                CONTAINERD_SOCKET_CANDIDATES
                    .iter()
                    .find(|p| Path::new(p).exists())
                    .unwrap_or(&CONTAINERD_SOCKET_CANDIDATES[0])
            ),
        }
    } else {
        let socket_paths = CONTAINERD_SOCKET_CANDIDATES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        ServiceStatus {
            name: "containerd".to_string(),
            status: SERVICE_NOT_READY.to_string(),
            detail: format!("no reachable socket found; checked: {}", socket_paths),
        }
    });

    // dockerd status — distinguish socket-connectable vs API-ready.
    let docker_detail = if docker_api_ok {
        format!("API /_ping OK: {}", DOCKER_API_UNIX_SOCKET)
    } else if docker_socket_ok {
        format!(
            "socket connectable, API initializing: {}",
            DOCKER_API_UNIX_SOCKET
        )
    } else if Path::new(DOCKER_API_UNIX_SOCKET).exists() {
        format!(
            "socket exists but not connectable: {}",
            DOCKER_API_UNIX_SOCKET
        )
    } else {
        format!("socket missing: {}", DOCKER_API_UNIX_SOCKET)
    };

    services.push(ServiceStatus {
        name: "dockerd".to_string(),
        status: if docker_ready {
            SERVICE_READY.to_string()
        } else if Path::new(DOCKER_API_UNIX_SOCKET).exists() {
            SERVICE_ERROR.to_string()
        } else {
            SERVICE_NOT_READY.to_string()
        },
        detail: docker_detail,
    });

    // Build the summary detail string.
    let detail = if docker_ready {
        "docker socket ready".to_string()
    } else if Path::new(DOCKER_API_UNIX_SOCKET).exists() {
        format!(
            "docker socket exists but not reachable: {}",
            DOCKER_API_UNIX_SOCKET
        )
    } else if !missing_runtime_binaries.is_empty() {
        format!(
            "docker socket missing: {}; {}",
            DOCKER_API_UNIX_SOCKET,
            runtime_missing_detail_from(&missing_runtime_binaries)
        )
    } else {
        format!("docker socket missing: {}", DOCKER_API_UNIX_SOCKET)
    };

    RuntimeStatusResponse {
        containerd_ready,
        docker_ready,
        endpoint: format!("vsock:{}", docker_api_vsock_port()),
        detail,
        services,
    }
}

async fn handle_start_kubernetes(_req: KubernetesStartRequest) -> RpcResponse {
    RpcResponse::KubernetesStart(do_start_kubernetes().await)
}

async fn handle_stop_kubernetes(_req: KubernetesStopRequest) -> RpcResponse {
    RpcResponse::KubernetesStop(do_stop_kubernetes().await)
}

async fn handle_delete_kubernetes(_req: KubernetesDeleteRequest) -> RpcResponse {
    RpcResponse::KubernetesDelete(do_delete_kubernetes().await)
}

async fn handle_kubernetes_status(_req: KubernetesStatusRequest) -> RpcResponse {
    let _guard = kubernetes_control_lock().lock().await;
    RpcResponse::KubernetesStatus(collect_kubernetes_status().await)
}

async fn handle_kubernetes_kubeconfig(_req: KubernetesKubeconfigRequest) -> RpcResponse {
    match std::fs::read_to_string(K3S_KUBECONFIG_PATH) {
        Ok(kubeconfig) => RpcResponse::KubernetesKubeconfig(KubernetesKubeconfigResponse {
            kubeconfig,
            context_name: "arcbox".to_string(),
            endpoint: KUBERNETES_HOST_ENDPOINT.to_string(),
        }),
        Err(e) => RpcResponse::Error(ErrorResponse::new(
            404,
            format!("kubeconfig unavailable: {e}"),
        )),
    }
}

async fn do_start_kubernetes() -> KubernetesStartResponse {
    let _guard = kubernetes_control_lock().lock().await;
    let runtime_bin_dir = PathBuf::from(ARCBOX_RUNTIME_BIN_DIR);
    let missing = missing_kubernetes_binaries_at(&runtime_bin_dir);
    if !missing.is_empty() {
        return KubernetesStartResponse {
            running: false,
            api_ready: false,
            endpoint: KUBERNETES_HOST_ENDPOINT.to_string(),
            detail: format!(
                "missing kubernetes binaries under {}: {}",
                ARCBOX_RUNTIME_BIN_DIR,
                missing.join(", ")
            ),
        };
    }

    let mut notes = ensure_runtime_prerequisites();
    match ensure_data_mount() {
        Ok(note) => notes.push(note),
        Err(e) => {
            return KubernetesStartResponse {
                running: false,
                api_ready: false,
                endpoint: KUBERNETES_HOST_ENDPOINT.to_string(),
                detail: format!("data volume setup failed: {e}"),
            };
        }
    }
    ensure_shared_runtime_dirs(&mut notes);

    if !ensure_containerd_ready(&runtime_bin_dir, &mut notes).await {
        return KubernetesStartResponse {
            running: false,
            api_ready: false,
            endpoint: KUBERNETES_HOST_ENDPOINT.to_string(),
            detail: notes.join("; "),
        };
    }

    if k3s_pid().is_none() {
        let k3s_bin = runtime_bin_dir.join("k3s");
        let mut cmd = Command::new(&k3s_bin);
        cmd.arg("server")
            .arg(format!("--container-runtime-endpoint={CONTAINERD_SOCKET}"))
            .arg(format!("--data-dir={K3S_DATA_MOUNT_POINT}"))
            .arg(format!("--write-kubeconfig={K3S_KUBECONFIG_PATH}"))
            .arg("--write-kubeconfig-mode=0600")
            .arg("--disable=traefik")
            .arg("--disable=servicelb")
            .env("PATH", runtime_path_env(&runtime_bin_dir))
            .stdin(Stdio::null())
            .stdout(daemon_log_file("k3s"))
            .stderr(daemon_log_file("k3s"));

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id().unwrap_or_default();
                if let Err(e) = write_pid_file(K3S_PID_FILE, pid) {
                    notes.push(format!("write k3s pid file failed({})", e));
                }
                notes.push(format!("spawned k3s (pid={pid})"));
            }
            Err(e) => {
                notes.push(format!("failed to spawn k3s: {}", e));
            }
        }
    } else {
        notes.push("k3s already running".to_string());
    }

    let mut status = collect_kubernetes_status().await;
    for _ in 0..60 {
        if status.api_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        status = collect_kubernetes_status().await;
    }

    if !notes.is_empty() {
        status.detail = format!("{}; {}", notes.join("; "), status.detail);
    }
    KubernetesStartResponse {
        running: status.running,
        api_ready: status.api_ready,
        endpoint: status.endpoint,
        detail: status.detail,
    }
}

async fn do_stop_kubernetes() -> KubernetesStopResponse {
    let _guard = kubernetes_control_lock().lock().await;
    do_stop_kubernetes_locked().await
}

async fn do_stop_kubernetes_locked() -> KubernetesStopResponse {
    let Some(pid) = k3s_pid() else {
        return KubernetesStopResponse {
            stopped: true,
            detail: "k3s already stopped".to_string(),
        };
    };

    let pid = nix::unistd::Pid::from_raw(pid);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    for _ in 0..20 {
        if !process_alive(pid.as_raw()) {
            remove_pid_file(K3S_PID_FILE);
            return KubernetesStopResponse {
                stopped: true,
                detail: "k3s stopped".to_string(),
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    remove_pid_file(K3S_PID_FILE);
    KubernetesStopResponse {
        stopped: true,
        detail: "k3s force-stopped".to_string(),
    }
}

async fn do_delete_kubernetes() -> KubernetesDeleteResponse {
    let _guard = kubernetes_control_lock().lock().await;
    let stop = do_stop_kubernetes_locked().await;
    let mut notes = vec![stop.detail];
    clear_kubernetes_namespace(&mut notes).await;

    for path in [
        K3S_DATA_MOUNT_POINT,
        KUBELET_DATA_MOUNT_POINT,
        CNI_DATA_MOUNT_POINT,
    ] {
        if let Err(e) = clear_directory_contents(Path::new(path)) {
            notes.push(format!("clear {} failed({})", path, e));
        }
    }

    KubernetesDeleteResponse {
        detail: notes.join("; "),
    }
}

async fn clear_kubernetes_namespace(notes: &mut Vec<String>) {
    let k3s_bin = PathBuf::from(ARCBOX_RUNTIME_BIN_DIR).join("k3s");
    if !k3s_bin.exists() {
        return;
    }

    match Command::new(&k3s_bin)
        .arg("ctr")
        .arg("--address")
        .arg(CONTAINERD_SOCKET)
        .arg("namespaces")
        .arg("remove")
        .arg(K3S_NAMESPACE)
        .status()
        .await
    {
        Ok(status) if status.success() => {
            notes.push("removed containerd namespace k8s.io".to_string());
        }
        Ok(status) => {
            notes.push(format!(
                "remove containerd namespace k8s.io exit={}",
                status.code().unwrap_or(-1)
            ));
        }
        Err(e) => {
            notes.push(format!("remove containerd namespace k8s.io failed({})", e));
        }
    }
}

fn clear_directory_contents(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            std::fs::remove_dir_all(&entry_path)?;
        } else {
            std::fs::remove_file(&entry_path)?;
        }
    }

    Ok(())
}

async fn collect_kubernetes_status() -> KubernetesStatusResponse {
    use arcbox_protocol::agent::ServiceStatus;

    let containerd_ready = probe_first_ready_socket(&CONTAINERD_SOCKET_CANDIDATES).await;
    let running = k3s_pid().is_some();
    let api_ready = k3s_api_ready().await;

    let mut services = Vec::new();
    services.push(ServiceStatus {
        name: "containerd".to_string(),
        status: if containerd_ready {
            SERVICE_READY.to_string()
        } else {
            SERVICE_NOT_READY.to_string()
        },
        detail: if containerd_ready {
            CONTAINERD_SOCKET.to_string()
        } else {
            "containerd socket not reachable".to_string()
        },
    });
    services.push(ServiceStatus {
        name: "k3s".to_string(),
        status: if api_ready {
            SERVICE_READY.to_string()
        } else if running {
            SERVICE_ERROR.to_string()
        } else {
            SERVICE_NOT_READY.to_string()
        },
        detail: if api_ready {
            format!("api reachable on 127.0.0.1:{KUBERNETES_API_GUEST_PORT}")
        } else if running {
            "k3s process running but api not ready".to_string()
        } else {
            "k3s process not running".to_string()
        },
    });

    let detail = if api_ready {
        "kubernetes api ready".to_string()
    } else if running {
        "k3s running but kubernetes api not ready".to_string()
    } else {
        "k3s not running".to_string()
    };

    KubernetesStatusResponse {
        running,
        api_ready,
        endpoint: KUBERNETES_HOST_ENDPOINT.to_string(),
        detail,
        services,
    }
}

fn runtime_start_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

const REQUIRED_RUNTIME_BINARIES: &[&str] =
    &["dockerd", "containerd", "containerd-shim-runc-v2", "runc"];
const REQUIRED_KUBERNETES_BINARIES: &[&str] =
    &["containerd", "containerd-shim-runc-v2", "runc", "k3s"];

fn detect_runtime_bin_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(ARCBOX_RUNTIME_BIN_DIR);
    if missing_runtime_binaries_at(&dir).is_empty() {
        Some(dir)
    } else {
        None
    }
}

fn runtime_missing_detail() -> String {
    let dir = PathBuf::from(ARCBOX_RUNTIME_BIN_DIR);
    let missing = missing_runtime_binaries_at(&dir);
    runtime_missing_detail_from(&missing)
}

fn runtime_missing_detail_from(missing: &[&'static str]) -> String {
    if missing.is_empty() {
        format!("all runtime binaries present under {ARCBOX_RUNTIME_BIN_DIR}")
    } else {
        format!(
            "missing runtime binaries under {}: {}",
            ARCBOX_RUNTIME_BIN_DIR,
            missing.join(", ")
        )
    }
}

fn missing_runtime_binaries_at(dir: &Path) -> Vec<&'static str> {
    missing_binaries_at(dir, REQUIRED_RUNTIME_BINARIES)
}

fn missing_kubernetes_binaries_at(dir: &Path) -> Vec<&'static str> {
    missing_binaries_at(dir, REQUIRED_KUBERNETES_BINARIES)
}

fn missing_binaries_at(dir: &Path, required: &[&'static str]) -> Vec<&'static str> {
    required
        .iter()
        .copied()
        .filter(|name| !dir.join(name).exists())
        .collect()
}

/// Ensures the guest environment has the prerequisites that dockerd/containerd
/// need: cgroup2, overlayfs, devpts, /dev/shm, /tmp, /run.
fn ensure_runtime_prerequisites() -> Vec<String> {
    let mut notes = Vec::new();

    // Use /bin/busybox <applet> directly — always present on EROFS rootfs.
    let busybox = "/bin/busybox";

    // Mount cgroup2 unified hierarchy (required by dockerd).
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        if let Err(e) = std::fs::create_dir_all("/sys/fs/cgroup") {
            notes.push(format!("mkdir /sys/fs/cgroup failed({})", e));
        } else {
            let rc = std::process::Command::new(busybox)
                .args(["mount", "-t", "cgroup2", "cgroup2", "/sys/fs/cgroup"])
                .status();
            match rc {
                Ok(s) if s.success() => notes.push("mounted cgroup2".to_string()),
                Ok(s) => notes.push(format!("mount cgroup2 exit={}", s.code().unwrap_or(-1))),
                Err(e) => notes.push(format!("mount cgroup2 failed({})", e)),
            }
        }
    }

    // Mount devpts if missing (needed for PTY allocation).
    if !Path::new("/dev/pts/ptmx").exists() {
        let _ = std::fs::create_dir_all("/dev/pts");
        let _ = std::process::Command::new(busybox)
            .args([
                "mount",
                "-t",
                "devpts",
                "-o",
                "gid=5,mode=0620,noexec,nosuid",
                "devpts",
                "/dev/pts",
            ])
            .status();
    }

    // Mount /dev/shm if missing.
    if !Path::new("/dev/shm").exists() {
        let _ = std::fs::create_dir_all("/dev/shm");
        let _ = std::process::Command::new(busybox)
            .args([
                "mount",
                "-t",
                "tmpfs",
                "-o",
                "nodev,nosuid,noexec",
                "shm",
                "/dev/shm",
            ])
            .status();
    }

    // Ensure /tmp and /run exist as writable tmpfs.
    for dir in ["/tmp", "/run"] {
        if !Path::new(dir).exists()
            || std::fs::metadata(dir).is_ok_and(|m| m.permissions().readonly())
        {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::process::Command::new(busybox)
                .args(["mount", "-t", "tmpfs", "tmpfs", dir])
                .status();
        }
    }

    // Enable IPv4 forwarding so Docker can route traffic between docker0 and eth0.
    // VZ framework NAT masquerades all VM traffic, so no guest-side masquerade rule needed.
    if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n") {
        notes.push(format!("ip_forward failed({})", e));
    } else {
        notes.push("enabled ip_forward".to_string());
    }

    // Load overlay module (needed for Docker's overlay2 storage driver).
    if !Path::new("/sys/module/overlay").exists() {
        let rc = std::process::Command::new("/sbin/modprobe")
            .arg("overlay")
            .status();
        match rc {
            Ok(s) if s.success() => notes.push("loaded overlay module".to_string()),
            _ => {
                // Fallback: try insmod with kernel version path.
                if let Ok(uname) = std::process::Command::new(busybox)
                    .arg("uname")
                    .arg("-r")
                    .output()
                {
                    let kver = String::from_utf8_lossy(&uname.stdout).trim().to_string();
                    let ko = format!("/lib/modules/{}/kernel/fs/overlayfs/overlay.ko", kver);
                    if Path::new(&ko).exists() {
                        let _ = std::process::Command::new(busybox)
                            .args(["insmod", &ko])
                            .status();
                        notes.push(format!("insmod overlay from {}", ko));
                    } else {
                        notes.push("overlay module not found".to_string());
                    }
                }
            }
        }
    }

    // Clock guard: if the wall clock is still near epoch, the host Ping
    // (which carries the real timestamp) hasn't arrived yet. Set it to a
    // known-safe minimum so TLS validation in dockerd/containerd doesn't
    // fail with "certificate is not yet valid". The next Ping overwrites
    // this with the real host time.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let min_epoch = option_env!("SOURCE_DATE_EPOCH")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        .max(MIN_SANE_EPOCH);
    if now_secs < min_epoch {
        if sync_clock_from_host(min_epoch as i64) {
            notes.push("clock guard: set to minimum sane time (pre-ping fallback)".to_string());
        } else {
            notes.push("clock guard: failed to set clock (pre-ping fallback)".to_string());
        }
    }

    notes
}

/// Minimum "sane" UNIX timestamp for TLS certificate validation.
///
/// Chosen to be old enough that it's always in the past, but recent
/// enough that TLS certificates issued after 2020 pass validation.
/// 2020-01-01T00:00:00Z
const MIN_SANE_EPOCH: u64 = 1_577_836_800;

/// Redirects daemon stdout/stderr to a log file so crashes are diagnosable.
///
/// Prefers `/arcbox/log/` (VirtioFS mount, visible from host as `~/.arcbox/log/`)
/// so that logs survive guest restarts and are accessible without exec.
/// Falls back to `/tmp/` (guest tmpfs) if VirtioFS is not mounted.
fn daemon_log_file(name: &str) -> Stdio {
    let log_dir = format!("/arcbox/{}", arcbox_constants::paths::guest::LOG);
    let arcbox_path = format!("{}/{}.log", log_dir, name);
    let tmp_log_path = format!("/tmp/{}.log", name);

    // Ensure the log directory exists inside the VirtioFS share.
    if Path::new("/arcbox").exists() {
        let _ = std::fs::create_dir_all(&log_dir);
    }

    let log_path = if Path::new("/arcbox").exists() {
        &arcbox_path
    } else {
        &tmp_log_path
    };

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => f.into(),
        Err(_) => {
            // Fallback to /tmp/ if /arcbox/log/ write fails.
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&tmp_log_path)
            {
                Ok(f) => f.into(),
                Err(_) => Stdio::null(),
            }
        }
    }
}

async fn try_start_bundled_runtime() -> String {
    let _guard = runtime_start_lock().lock().await;

    if probe_unix_socket(DOCKER_API_UNIX_SOCKET).await {
        return "docker socket already ready".to_string();
    }

    let Some(runtime_bin_dir) = detect_runtime_bin_dir() else {
        return runtime_missing_detail();
    };

    tracing::info!(
        runtime_bin_dir = %runtime_bin_dir.display(),
        "starting bundled runtime"
    );

    let mut notes = Vec::new();

    // Ensure kernel/filesystem prerequisites before spawning daemons.
    let prereq_notes = ensure_runtime_prerequisites();
    if !prereq_notes.is_empty() {
        tracing::info!(prerequisites = %prereq_notes.join("; "), "runtime prerequisites");
    }
    notes.extend(prereq_notes);
    match ensure_data_mount() {
        Ok(note) => notes.push(note),
        Err(e) => return format!("data volume setup failed: {}", e),
    }

    ensure_shared_runtime_dirs(&mut notes);

    if !ensure_containerd_ready(&runtime_bin_dir, &mut notes).await {
        return notes.join("; ");
    }

    ensure_dockerd_ready(&runtime_bin_dir, &mut notes).await;

    notes.join("; ")
}
