//! Agent main loop and request handling.
//!
//! The Agent listens on vsock port 1024 and handles RPC requests from the host.
//! It manages container lifecycle and executes commands in the guest VM.

use anyhow::Result;
use arcbox_constants::ports::AGENT_PORT;

pub mod ensure_runtime;

// =============================================================================
// Linux Implementation
// =============================================================================

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::TcpStream;
    use tokio::net::UnixStream;
    use tokio::process::Command;
    use tokio::sync::Mutex;
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

    use super::AGENT_PORT;
    use super::ensure_runtime;
    use crate::rpc::{
        AGENT_VERSION, ErrorResponse, MessageType, RpcRequest, RpcResponse, parse_request,
        read_message, write_message, write_response,
    };
    use crate::sandbox::SandboxService;
    use arcbox_constants::cmdline::{
        DOCKER_DATA_DEVICE_KEY as DOCKER_DATA_DEVICE_CMDLINE_KEY, GUEST_DOCKER_VSOCK_PORT_KEY,
    };
    use arcbox_constants::devices::DOCKER_DATA_BLOCK_DEVICE as DOCKER_DATA_DEVICE_DEFAULT;
    use arcbox_constants::env::GUEST_DOCKER_VSOCK_PORT as GUEST_DOCKER_VSOCK_PORT_ENV;
    use arcbox_constants::paths::{
        ARCBOX_RUNTIME_BIN_DIR, CNI_DATA_MOUNT_POINT, CONTAINERD_DATA_MOUNT_POINT,
        CONTAINERD_SOCKET, DOCKER_API_UNIX_SOCKET, DOCKER_DATA_MOUNT_POINT, K3S_CNI_BIN_DIR,
        K3S_CNI_CONF_DIR, K3S_DATA_MOUNT_POINT, K3S_KUBECONFIG_PATH, KUBELET_DATA_MOUNT_POINT,
    };
    use arcbox_constants::ports::{
        DOCKER_API_VSOCK_PORT, KUBERNETES_API_GUEST_PORT, KUBERNETES_API_VSOCK_PORT,
    };
    use arcbox_constants::status::{SERVICE_ERROR, SERVICE_NOT_READY, SERVICE_READY};

    use arcbox_protocol::agent::{
        KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesKubeconfigRequest,
        KubernetesKubeconfigResponse, KubernetesStartRequest, KubernetesStartResponse,
        KubernetesStatusRequest, KubernetesStatusResponse, KubernetesStopRequest,
        KubernetesStopResponse, PingResponse, RuntimeEnsureRequest, RuntimeEnsureResponse,
        RuntimeStatusRequest, RuntimeStatusResponse, SystemInfo,
    };

    // =========================================================================
    // Sandbox service singleton
    // =========================================================================

    /// Returns the global [`SandboxService`] singleton.
    ///
    /// The service is initialised lazily on the first call.  Initialisation
    /// failures are logged as warnings and `None` is stored, so sandbox
    /// operations will return a 503 error rather than crashing the agent.
    fn sandbox_service() -> Option<&'static Arc<SandboxService>> {
        static SERVICE: OnceLock<Option<Arc<SandboxService>>> = OnceLock::new();
        SERVICE
            .get_or_init(|| {
                let config = crate::config::load();
                match SandboxService::new(config) {
                    Ok(svc) => {
                        tracing::info!("sandbox service initialised");
                        Some(Arc::new(svc))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sandbox service unavailable");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Containerd socket candidates (primary + legacy fallback).
    const CONTAINERD_SOCKET_CANDIDATES: [&str; 2] =
        [CONTAINERD_SOCKET, "/var/run/containerd/containerd.sock"];
    const K3S_PID_FILE: &str = "/run/arcbox/k3s.pid";
    const K3S_NAMESPACE: &str = "k8s.io";
    const KUBERNETES_HOST_ENDPOINT: &str = "https://127.0.0.1:16443";

    fn cmdline_value(key: &str) -> Option<String> {
        let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
        for token in cmdline.split_whitespace() {
            if let Some(value) = token.strip_prefix(key) {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    }

    fn docker_api_vsock_port() -> u32 {
        if let Some(port) = std::env::var(GUEST_DOCKER_VSOCK_PORT_ENV)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|port| *port > 0)
        {
            return port;
        }

        if let Some(port) = cmdline_value(GUEST_DOCKER_VSOCK_PORT_KEY)
            .and_then(|raw| raw.parse::<u32>().ok())
            .filter(|port| *port > 0)
        {
            return port;
        }

        DOCKER_API_VSOCK_PORT
    }

    /// HVC fast-path block device for the data disk (device index 1 = vdb).
    /// Falls back to the standard VirtIO block device if HVC device is absent.
    const HVC_DATA_DEVICE: &str = "/dev/arcboxhvc1";

    fn docker_data_device() -> String {
        // Prefer explicit kernel cmdline override.
        if let Some(v) = cmdline_value(DOCKER_DATA_DEVICE_CMDLINE_KEY) {
            if !v.trim().is_empty() {
                return v;
            }
        }
        // Use HVC fast-path device if available.
        if Path::new(HVC_DATA_DEVICE).exists() {
            tracing::info!("using HVC fast-path block device: {}", HVC_DATA_DEVICE);
            return HVC_DATA_DEVICE.to_string();
        }
        DOCKER_DATA_DEVICE_DEFAULT.to_string()
    }

    fn kubernetes_api_vsock_port() -> u32 {
        KUBERNETES_API_VSOCK_PORT
    }

    /// Btrfs primary superblock magic `_BHRfS_M` at absolute disk offset
    /// `0x10040` (superblock starts at `0x10000`, magic at internal offset `0x40`).
    const BTRFS_MAGIC: [u8; 8] = [0x5f, 0x42, 0x48, 0x52, 0x66, 0x53, 0x5f, 0x4d];
    const BTRFS_MAGIC_OFFSET: u64 = 0x10040;

    /// Temporary mount point for the raw Btrfs device before subvolume bind mounts.
    ///
    /// Must live on a writable filesystem. `/run` is tmpfs (set up in PID1 init),
    /// while EROFS root is read-only and cannot host dynamic mountpoints.
    const BTRFS_TEMP_MOUNT: &str = "/run/arcbox/data";

    fn has_btrfs_superblock(device: &str) -> bool {
        let mut file = match std::fs::File::open(device) {
            Ok(file) => file,
            Err(_) => return false,
        };
        if file.seek(SeekFrom::Start(BTRFS_MAGIC_OFFSET)).is_err() {
            return false;
        }
        let mut magic = [0_u8; 8];
        if file.read_exact(&mut magic).is_err() {
            return false;
        }
        magic == BTRFS_MAGIC
    }

    /// Formats the device as Btrfs if it does not already have a Btrfs superblock.
    /// Old ext4 disks are unconditionally wiped (alpha breaking change).
    fn ensure_btrfs_format(device: &str) -> Result<String, String> {
        if has_btrfs_superblock(device) {
            return Ok("data device already Btrfs".to_string());
        }

        // /sbin/mkfs.btrfs is baked into the EROFS rootfs.
        let binary = "/sbin/mkfs.btrfs";
        if !Path::new(binary).exists() {
            return Err(format!("{} not found in EROFS rootfs", binary));
        }

        match std::process::Command::new(binary)
            .args(["-f", device])
            .status()
        {
            Ok(status) if status.success() => Ok(format!("formatted {} as Btrfs", device)),
            Ok(status) => Err(format!(
                "mkfs.btrfs failed on {} (exit={})",
                device,
                status.code().unwrap_or(-1)
            )),
            Err(e) => Err(format!("failed to execute mkfs.btrfs: {}", e)),
        }
    }

    /// Mounts the data volume (Btrfs), creates subvolumes, and bind-mounts them.
    ///
    /// Layout after this function returns:
    /// - `/run/arcbox/data` — raw Btrfs mount (internal, not used by daemons)
    /// - `/var/lib/docker` — bind mount of `@docker` subvolume
    /// - `/var/lib/containerd` — bind mount of `@containerd` subvolume
    /// - `/var/lib/rancher/k3s` — bind mount of `@k3s` subvolume
    /// - `/var/lib/kubelet` — bind mount of `@kubelet` subvolume
    /// - `/var/lib/cni` — bind mount of `@cni` subvolume
    ///
    /// Returns `Ok(notes)` on success or `Err(reason)` if the data volume
    /// could not be set up. Callers must abort runtime startup on error —
    /// running containerd/dockerd without persistent storage is unsafe.
    fn ensure_data_mount() -> Result<String, String> {
        // Already fully set up?
        if crate::mount::is_mounted(DOCKER_DATA_MOUNT_POINT)
            && crate::mount::is_mounted(CONTAINERD_DATA_MOUNT_POINT)
            && crate::mount::is_mounted(K3S_DATA_MOUNT_POINT)
            && crate::mount::is_mounted(KUBELET_DATA_MOUNT_POINT)
            && crate::mount::is_mounted(CNI_DATA_MOUNT_POINT)
        {
            return Ok("data subvolumes already mounted".to_string());
        }

        let device = docker_data_device();
        if !Path::new(&device).exists() {
            return Err(format!("data device missing: {}", device));
        }

        // Step 1: Format if not Btrfs.
        match ensure_btrfs_format(&device) {
            Ok(note) => tracing::info!("{}", note),
            Err(e) => return Err(e),
        }

        // Step 2: Mount raw Btrfs to temporary writable mount point.
        if !crate::mount::is_mounted(BTRFS_TEMP_MOUNT) {
            if let Err(e) = std::fs::create_dir_all(BTRFS_TEMP_MOUNT) {
                return Err(format!("failed to create {}: {}", BTRFS_TEMP_MOUNT, e));
            }
            match std::process::Command::new("/bin/busybox")
                .args([
                    "mount",
                    "-t",
                    "btrfs",
                    "-o",
                    "compress=zstd:3,discard=async",
                    &device,
                    BTRFS_TEMP_MOUNT,
                ])
                .status()
            {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    return Err(format!(
                        "mount -t btrfs {} {} failed (exit={})",
                        device,
                        BTRFS_TEMP_MOUNT,
                        s.code().unwrap_or(-1)
                    ));
                }
                Err(e) => return Err(format!("mount exec failed: {}", e)),
            }
        }

        // Step 3: Create subvolumes if missing.
        for subvol in ["@docker", "@containerd", "@k3s", "@kubelet", "@cni"] {
            let subvol_path = format!("{}/{}", BTRFS_TEMP_MOUNT, subvol);
            if Path::new(&subvol_path).exists() {
                continue;
            }
            // EROFS only includes mkfs.btrfs, not full btrfs-progs. Use the
            // BTRFS_IOC_SUBVOL_CREATE ioctl directly to create subvolumes.
            if let Err(e) = btrfs_create_subvolume(&subvol_path) {
                return Err(format!("failed to create subvolume {}: {}", subvol, e));
            }
        }

        let mut notes = Vec::new();

        // Step 4: Bind mount subvolumes to final paths.
        for (subvol, target) in [
            ("@docker", DOCKER_DATA_MOUNT_POINT),
            ("@containerd", CONTAINERD_DATA_MOUNT_POINT),
            ("@k3s", K3S_DATA_MOUNT_POINT),
            ("@kubelet", KUBELET_DATA_MOUNT_POINT),
            ("@cni", CNI_DATA_MOUNT_POINT),
        ] {
            if crate::mount::is_mounted(target) {
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(target) {
                return Err(format!("failed to create {}: {}", target, e));
            }
            let opts = format!(
                "compress=zstd:1,discard=async,noatime,space_cache=v2,subvol={}",
                subvol
            );
            match std::process::Command::new("/bin/busybox")
                .args(["mount", "-t", "btrfs", "-o", &opts, &device, target])
                .status()
            {
                Ok(s) if s.success() => {
                    // Disable Btrfs COW on metadata-heavy subdirectories.
                    // BoltDB (containerd/dockerd) does frequent fdatasync on
                    // small pages. Without NOCOW, each write triggers Btrfs
                    // copy-on-write + APFS COW on the host = double amplification.
                    disable_cow_on_metadata_dirs(target);
                    notes.push(format!("mounted {} -> {}", subvol, target));
                }
                Ok(s) => {
                    return Err(format!(
                        "mount subvol={} {} failed (exit={})",
                        subvol,
                        target,
                        s.code().unwrap_or(-1)
                    ));
                }
                Err(e) => return Err(format!("mount exec failed: {}", e)),
            }
        }

        if notes.is_empty() {
            Ok("data subvolumes already mounted".to_string())
        } else {
            Ok(notes.join("; "))
        }
    }

    /// Disables Btrfs COW (sets NOCOW attribute) on metadata-heavy subdirectories.
    ///
    /// BoltDB and other metadata stores do frequent fdatasync on small pages.
    /// Btrfs COW amplifies each write (copy 16KB metadata page + update B-tree),
    /// and the host's APFS does another COW on top — double write amplification.
    /// NOCOW converts these to in-place overwrites at the Btrfs layer.
    #[cfg(target_os = "linux")]
    fn disable_cow_on_metadata_dirs(mount_point: &str) {
        // FS_IOC_SETFLAGS = _IOW('f', 2, long)
        // FS_NOCOW_FL = 0x00800000
        const FS_NOCOW_FL: libc::c_long = 0x0080_0000;

        // Subdirectories that contain BoltDB or other fsync-heavy metadata.
        // The NOCOW attribute is inherited by new files created in these dirs.
        let metadata_subdirs = [
            "io.containerd.metadata.v1.bolt",
            "io.containerd.snapshotter.v1.overlayfs",
            "containerd",
            "network",
            "builder",
            "buildkit",
            "image",
            "trust",
        ];

        for subdir in &metadata_subdirs {
            let path = format!("{}/{}", mount_point, subdir);
            let _ = std::fs::create_dir_all(&path);

            let Ok(cpath) = std::ffi::CString::new(path.as_str()) else {
                continue;
            };
            // SAFETY: valid path, O_RDONLY | O_DIRECTORY.
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
            if fd < 0 {
                continue;
            }

            let mut flags: libc::c_long = 0;
            // Get current flags, then set NOCOW.
            // SAFETY: FS_IOC_GETFLAGS/SETFLAGS on a valid directory fd.
            unsafe {
                #[allow(clippy::cast_possible_truncation)]
                let get_flags: libc::c_ulong = 0x8008_6601; // FS_IOC_GETFLAGS
                #[allow(clippy::cast_possible_truncation)]
                let set_flags: libc::c_ulong = 0x4008_6602; // FS_IOC_SETFLAGS
                if libc::ioctl(fd, get_flags as libc::c_int, &mut flags) == 0 {
                    flags |= FS_NOCOW_FL;
                    if libc::ioctl(fd, set_flags as libc::c_int, &flags) == 0 {
                        tracing::debug!("set NOCOW on {}", path);
                    }
                }
                libc::close(fd);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn disable_cow_on_metadata_dirs(_mount_point: &str) {}

    // BTRFS_IOC_SUBVOL_CREATE = _IOW(0x94, 14, struct btrfs_ioctl_vol_args)
    // struct btrfs_ioctl_vol_args { __s64 fd; char name[4088]; }  total = 4096 bytes
    //
    // nix::ioctl_write_ptr! computes the request number portably (handles
    // c_int on musl vs c_ulong on glibc).
    #[cfg(target_os = "linux")]
    nix::ioctl_write_ptr!(btrfs_ioc_subvol_create, 0x94, 14, [u8; 4096]);

    /// Creates a Btrfs subvolume using the `BTRFS_IOC_SUBVOL_CREATE` ioctl.
    ///
    /// This avoids needing the full `btrfs-progs` CLI in the EROFS rootfs.
    #[cfg(target_os = "linux")]
    fn btrfs_create_subvolume(path: &str) -> Result<(), String> {
        use std::os::unix::io::AsRawFd;

        let parent = Path::new(path)
            .parent()
            .ok_or_else(|| "no parent directory".to_string())?;
        let name = Path::new(path)
            .file_name()
            .ok_or_else(|| "no subvolume name".to_string())?
            .to_str()
            .ok_or_else(|| "invalid subvolume name".to_string())?;

        let parent_dir =
            std::fs::File::open(parent).map_err(|e| format!("open {}: {}", parent.display(), e))?;

        let mut args = [0u8; 4096];
        // First 8 bytes: fd field (unused for SUBVOL_CREATE, set to 0).
        // Bytes 8..4096: null-terminated name.
        let name_bytes = name.as_bytes();
        if name_bytes.len() >= 4088 {
            return Err("subvolume name too long".to_string());
        }
        args[8..8 + name_bytes.len()].copy_from_slice(name_bytes);

        // SAFETY: valid fd from File::open, args buffer is 4096 bytes matching
        // the kernel struct btrfs_ioctl_vol_args layout.
        unsafe { btrfs_ioc_subvol_create(parent_dir.as_raw_fd(), &args) }
            .map_err(|e| format!("BTRFS_IOC_SUBVOL_CREATE: {}", e))?;

        tracing::info!("created Btrfs subvolume {}", path);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn btrfs_create_subvolume(_path: &str) -> Result<(), String> {
        Err("Btrfs subvolume creation is only supported on Linux".to_string())
    }

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

            let mut listener =
                bind_vsock_listener_with_retry(AGENT_PORT, "agent rpc listener").await?;

            tracing::info!("Agent listening on vsock port {}", AGENT_PORT);

            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        tracing::info!("Accepted connection from {:?}", peer_addr);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream).await {
                                tracing::error!("Connection error: {}", e);
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

    async fn bind_vsock_listener_with_retry(port: u32, component: &str) -> Result<VsockListener> {
        const INITIAL_DELAY_MS: u64 = 120;
        const MAX_DELAY_MS: u64 = 2_000;

        let mut delay_ms = INITIAL_DELAY_MS;

        loop {
            let addr = VsockAddr::new(VMADDR_CID_ANY, port);
            match VsockListener::bind(addr) {
                Ok(listener) => {
                    tracing::info!(port, component, "vsock listener bound");
                    return Ok(listener);
                }
                Err(e) => {
                    tracing::warn!(
                        port,
                        component,
                        retry_delay_ms = delay_ms,
                        error = %e,
                        "failed to bind vsock listener, retrying"
                    );
                }
            }

            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            delay_ms = (delay_ms * 3 / 2).min(MAX_DELAY_MS);
        }
    }

    async fn run_docker_api_proxy() -> Result<()> {
        let port = docker_api_vsock_port();
        let mut listener = bind_vsock_listener_with_retry(port, "docker api proxy").await?;
        tracing::info!("Docker API proxy listening on vsock port {}", port);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::info!("Docker API proxy accepted connection from {:?}", peer_addr);
                    tokio::spawn(async move {
                        if let Err(e) = proxy_docker_api_connection(stream).await {
                            tracing::warn!("Docker API proxy connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Docker API proxy accept failed: {}", e);
                }
            }
        }
    }

    async fn proxy_docker_api_connection(vsock_stream: VsockStream) -> Result<()> {
        let unix_stream = UnixStream::connect(DOCKER_API_UNIX_SOCKET)
            .await
            .context("failed to connect guest docker unix socket")?;
        tracing::info!("Docker proxy: connected to {}", DOCKER_API_UNIX_SOCKET);

        let (mut vsock_rd, mut vsock_wr) = tokio::io::split(vsock_stream);
        let (mut unix_rd, mut unix_wr) = tokio::io::split(unix_stream);

        // vsock → unix (host HTTP request → dockerd)
        let v2u = tokio::spawn(async move {
            let mut total: u64 = 0;
            let mut buf = [0u8; 8192];
            loop {
                let n = match tokio::io::AsyncReadExt::read(&mut vsock_rd, &mut buf).await {
                    Ok(0) => {
                        tracing::info!("Docker proxy vsock→unix: EOF after {} bytes", total);
                        break;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            "Docker proxy vsock→unix: read error after {} bytes: {}",
                            total,
                            e
                        );
                        break;
                    }
                };
                if total == 0 {
                    tracing::info!(
                        "Docker proxy vsock→unix: first chunk {} bytes: {:?}",
                        n,
                        String::from_utf8_lossy(&buf[..n.min(120)]),
                    );
                }
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut unix_wr, &buf[..n]).await {
                    tracing::warn!(
                        "Docker proxy vsock→unix: write error after {} bytes: {}",
                        total,
                        e
                    );
                    break;
                }
                total += n as u64;
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut unix_wr).await;
            total
        });

        // unix → vsock (dockerd response → host)
        let u2v = tokio::spawn(async move {
            let mut total: u64 = 0;
            let mut buf = [0u8; 8192];
            loop {
                let n = match tokio::io::AsyncReadExt::read(&mut unix_rd, &mut buf).await {
                    Ok(0) => {
                        tracing::info!("Docker proxy unix→vsock: EOF after {} bytes", total);
                        break;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            "Docker proxy unix→vsock: read error after {} bytes: {}",
                            total,
                            e
                        );
                        break;
                    }
                };
                if total == 0 {
                    tracing::info!(
                        "Docker proxy unix→vsock: first chunk {} bytes: {:?}",
                        n,
                        String::from_utf8_lossy(&buf[..n.min(120)]),
                    );
                }
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut vsock_wr, &buf[..n]).await
                {
                    tracing::warn!(
                        "Docker proxy unix→vsock: write error after {} bytes: {}",
                        total,
                        e
                    );
                    break;
                }
                total += n as u64;
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut vsock_wr).await;
            total
        });

        let (v2u_result, u2v_result) = tokio::join!(v2u, u2v);
        let v2u_bytes = v2u_result.unwrap_or(0);
        let u2v_bytes = u2v_result.unwrap_or(0);
        tracing::info!(
            "Docker proxy session done: vsock→unix={} bytes, unix→vsock={} bytes",
            v2u_bytes,
            u2v_bytes,
        );
        Ok(())
    }

    async fn run_kubernetes_api_proxy() -> Result<()> {
        let port = kubernetes_api_vsock_port();
        let mut listener = bind_vsock_listener_with_retry(port, "kubernetes api proxy").await?;
        tracing::info!("Kubernetes API proxy listening on vsock port {}", port);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::debug!(
                        "Kubernetes API proxy accepted connection from {:?}",
                        peer_addr
                    );
                    tokio::spawn(async move {
                        if let Err(e) = proxy_kubernetes_api_connection(stream).await {
                            tracing::debug!("Kubernetes API proxy connection ended: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Kubernetes API proxy accept failed: {}", e);
                }
            }
        }
    }

    async fn proxy_kubernetes_api_connection(mut vsock_stream: VsockStream) -> Result<()> {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, KUBERNETES_API_GUEST_PORT);
        let mut tcp_stream = TcpStream::connect(addr)
            .await
            .context("failed to connect guest kubernetes api socket")?;

        let _ = tokio::io::copy_bidirectional(&mut vsock_stream, &mut tcp_stream)
            .await
            .context("kubernetes api proxy copy failed")?;
        Ok(())
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
                if let Err(e) =
                    handle_sandbox_message(&mut stream, msg_type, &trace_id, &payload).await
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

    /// Dispatches a sandbox RPC request.
    ///
    /// Non-streaming requests (CRUD, snapshots) write a single response frame
    /// and return.  Streaming requests (Run, Events) write multiple frames
    /// until the stream ends.
    async fn handle_sandbox_message<S>(
        stream: &mut S,
        msg_type: MessageType,
        trace_id: &str,
        payload: &[u8],
    ) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let svc = match sandbox_service() {
            Some(s) => Arc::clone(s),
            None => {
                let err = ErrorResponse::new(503, "sandbox service unavailable");
                write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
                return Ok(());
            }
        };

        match msg_type {
            // -----------------------------------------------------------------
            // CRUD
            // -----------------------------------------------------------------
            MessageType::SandboxCreateRequest => match svc.create(payload).await {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxCreateResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxStopRequest => match svc.stop(payload).await {
                Ok(()) => {
                    write_message(stream, MessageType::SandboxStopResponse, trace_id, &[]).await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxRemoveRequest => match svc.remove(payload).await {
                Ok(()) => {
                    write_message(stream, MessageType::SandboxRemoveResponse, trace_id, &[])
                        .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxInspectRequest => match svc.inspect(payload) {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxInspectResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxListRequest => match svc.list(payload) {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxListResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            // -----------------------------------------------------------------
            // Streaming: Run
            // -----------------------------------------------------------------
            MessageType::SandboxRunRequest => {
                svc.handle_run(stream, trace_id, payload).await?;
            }
            // -----------------------------------------------------------------
            // Streaming: Events
            // -----------------------------------------------------------------
            MessageType::SandboxEventsRequest => {
                svc.handle_events(stream, trace_id, payload).await?;
            }
            // -----------------------------------------------------------------
            // Streaming: Exec
            // -----------------------------------------------------------------
            MessageType::SandboxExecRequest => {
                svc.handle_exec(stream, trace_id, payload).await?;
            }
            // -----------------------------------------------------------------
            // Snapshots
            // -----------------------------------------------------------------
            MessageType::SandboxCheckpointRequest => match svc.checkpoint(payload).await {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxCheckpointResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxRestoreRequest => match svc.restore(payload).await {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxRestoreResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxListSnapshotsRequest => match svc.list_snapshots(payload) {
                Ok(resp) => {
                    use prost::Message as _;
                    write_message(
                        stream,
                        MessageType::SandboxListSnapshotsResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            MessageType::SandboxDeleteSnapshotRequest => match svc.delete_snapshot(payload) {
                Ok(()) => {
                    write_message(
                        stream,
                        MessageType::SandboxDeleteSnapshotResponse,
                        trace_id,
                        &[],
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            },
            _ => {
                send_sandbox_error(stream, trace_id, 400, "unrecognised sandbox message type")
                    .await?;
            }
        }

        Ok(())
    }

    /// Write a single `Error` frame back to the caller.
    async fn send_sandbox_error<S>(
        stream: &mut S,
        trace_id: &str,
        code: i32,
        message: &str,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncWrite + Unpin,
    {
        let err = ErrorResponse::new(code, message);
        write_message(stream, MessageType::Error, trace_id, &err.encode()).await
    }

    /// Handles a single RPC request.
    async fn handle_request(request: RpcRequest) -> RequestResult {
        match request {
            RpcRequest::Ping(req) => RequestResult::Single(handle_ping(req)),
            RpcRequest::GetSystemInfo => RequestResult::Single(handle_get_system_info().await),
            RpcRequest::EnsureRuntime(req) => {
                RequestResult::Single(handle_ensure_runtime(req).await)
            }
            RpcRequest::RuntimeStatus(req) => {
                RequestResult::Single(handle_runtime_status(req).await)
            }
            RpcRequest::StartKubernetes(req) => {
                RequestResult::Single(handle_start_kubernetes(req).await)
            }
            RpcRequest::StopKubernetes(req) => {
                RequestResult::Single(handle_stop_kubernetes(req).await)
            }
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

    /// Handles a GetSystemInfo request.
    async fn handle_get_system_info() -> RpcResponse {
        let info = collect_system_info();
        RpcResponse::SystemInfo(info)
    }

    /// Idempotent, concurrency-safe EnsureRuntime handler.
    ///
    /// Delegates to the platform-independent `ensure_runtime` module, injecting
    /// the actual start and probe functions that depend on Linux system state.
    async fn handle_ensure_runtime(req: RuntimeEnsureRequest) -> RpcResponse {
        let guard = ensure_runtime::runtime_guard();

        let response = ensure_runtime::ensure_runtime(
            guard,
            req.start_if_needed,
            do_ensure_runtime_start(),
            do_ensure_runtime_probe(),
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

    async fn probe_tcp(addr: SocketAddrV4) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr)).await,
            Ok(Ok(_))
        )
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

    fn shared_containerd_config() -> String {
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

    async fn probe_first_ready_socket(paths: &[&str]) -> bool {
        for path in paths {
            if probe_unix_socket(path).await {
                return true;
            }
        }
        false
    }

    /// Checks if a Unix socket is connectable (lightweight probe).
    async fn probe_unix_socket(path: &str) -> bool {
        if !Path::new(path).exists() {
            return false;
        }
        match tokio::time::timeout(Duration::from_millis(300), UnixStream::connect(path)).await {
            Ok(Ok(_stream)) => true,
            Ok(Err(_)) | Err(_) => false,
        }
    }

    /// Sends a real HTTP `GET /_ping` to the Docker socket and waits for a
    /// valid response. This is much stronger than `probe_unix_socket` which
    /// only checks if `connect()` succeeds — dockerd may accept connections
    /// before its HTTP handler is ready.
    async fn probe_docker_api_ready(path: &str) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let Ok(Ok(mut stream)) =
            tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(path)).await
        else {
            return false;
        };
        let req = b"GET /_ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        if stream.write_all(req).await.is_err() {
            return false;
        }
        let mut buf = [0u8; 256];
        match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                let resp = String::from_utf8_lossy(&buf[..n]);
                // dockerd returns "HTTP/1.1 200 OK" with body "OK".
                resp.starts_with("HTTP/1.1 200")
            }
            _ => false,
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

    /// Collects system information from the guest.
    fn collect_system_info() -> SystemInfo {
        fn parse_ip_output(stdout: &[u8]) -> Vec<String> {
            let mut ips = Vec::new();
            let output = String::from_utf8_lossy(stdout);

            for token in output.split(|c: char| c.is_whitespace() || c == ',') {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }

                let Ok(addr) = token.parse::<IpAddr>() else {
                    continue;
                };
                if addr.is_loopback() {
                    continue;
                }

                let ip = addr.to_string();
                if !ips.iter().any(|existing| existing == &ip) {
                    ips.push(ip);
                }
            }

            ips
        }

        let mut info = SystemInfo::default();

        // Kernel version
        if let Ok(uname) = nix::sys::utsname::uname() {
            info.kernel_version = uname.release().to_string_lossy().to_string();
            info.os_name = uname.sysname().to_string_lossy().to_string();
            info.os_version = uname.version().to_string_lossy().to_string();
            info.arch = uname.machine().to_string_lossy().to_string();
            info.hostname = uname.nodename().to_string_lossy().to_string();
        }

        // Memory info
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            for line in meminfo.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(kb) = line.split_whitespace().nth(1) {
                        if let Ok(kb_val) = kb.parse::<u64>() {
                            info.total_memory = kb_val * 1024;
                        }
                    }
                } else if line.starts_with("MemAvailable:") {
                    if let Some(kb) = line.split_whitespace().nth(1) {
                        if let Ok(kb_val) = kb.parse::<u64>() {
                            info.available_memory = kb_val * 1024;
                        }
                    }
                }
            }
        }

        // CPU count
        info.cpu_count = std::thread::available_parallelism()
            .map(|p| p.get() as u32)
            .unwrap_or(1);

        // Load average
        if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
            let parts: Vec<&str> = loadavg.split_whitespace().collect();
            if parts.len() >= 3 {
                if let Ok(load1) = parts[0].parse::<f64>() {
                    info.load_average.push(load1);
                }
                if let Ok(load5) = parts[1].parse::<f64>() {
                    info.load_average.push(load5);
                }
                if let Ok(load15) = parts[2].parse::<f64>() {
                    info.load_average.push(load15);
                }
            }
        }

        // Uptime
        if let Ok(uptime) = std::fs::read_to_string("/proc/uptime") {
            if let Some(secs) = uptime.split_whitespace().next() {
                if let Ok(secs_val) = secs.parse::<f64>() {
                    info.uptime = secs_val as u64;
                }
            }
        }

        // IP addresses (excluding loopback).
        // Coreutils `hostname` supports `-I`, BusyBox supports `-i`.
        for flag in ["-I", "-i"] {
            let Ok(output) = std::process::Command::new("hostname").arg(flag).output() else {
                continue;
            };

            if !output.status.success() {
                continue;
            }

            let ips = parse_ip_output(&output.stdout);
            if !ips.is_empty() {
                info.ip_addresses = ips;
                break;
            }
        }

        info
    }
}
// =============================================================================
// macOS Stub Implementation (for development/testing)
// =============================================================================

#[cfg(not(target_os = "linux"))]
mod stub {
    use anyhow::Result;

    use super::AGENT_PORT;

    /// The Guest Agent (stub for non-Linux platforms).
    pub struct Agent;

    impl Agent {
        /// Creates a new agent.
        pub fn new() -> Self {
            Self
        }

        /// Runs the agent (stub mode).
        ///
        /// On non-Linux platforms (e.g., macOS), vsock is not available.
        /// This stub allows development and testing on the host.
        pub async fn run(&self) -> Result<()> {
            tracing::warn!("Agent is running in stub mode (non-Linux platform)");
            tracing::info!("Agent would listen on vsock port {}", AGENT_PORT);

            // In stub mode, we just keep the agent running
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                tracing::debug!("Agent stub heartbeat");
            }
        }
    }

    impl Default for Agent {
        fn default() -> Self {
            Self::new()
        }
    }
}

// =============================================================================
// Public API
// =============================================================================

#[cfg(target_os = "linux")]
pub use linux::Agent;

#[cfg(not(target_os = "linux"))]
pub use stub::Agent;

/// Runs the agent.
pub async fn run() -> Result<()> {
    let agent = Agent::new();
    agent.run().await
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Docker Log Format Parsing Tests
    // =========================================================================

    /// Helper to parse Docker JSON log line for testing.
    fn parse_docker_log_line(line: &str, stdout: bool, stderr: bool) -> Option<String> {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        let stream = parsed.get("stream")?.as_str()?;
        let log = parsed.get("log")?.as_str()?;

        match stream {
            "stdout" if stdout => Some(log.to_string()),
            "stderr" if stderr => Some(log.to_string()),
            _ => None,
        }
    }

    #[test]
    fn test_parse_docker_log_stdout() {
        let line = r#"{"log":"hello world","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, Some("hello world".to_string()));

        // Should filter out when stdout=false
        let result = parse_docker_log_line(line, false, true);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_docker_log_stderr() {
        let line = r#"{"log":"error message","stream":"stderr","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, false, true);
        assert_eq!(result, Some("error message".to_string()));

        // Should filter out when stderr=false
        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_docker_log_both_streams() {
        let stdout_line = r#"{"log":"stdout msg","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;
        let stderr_line = r#"{"log":"stderr msg","stream":"stderr","time":"2024-01-08T12:00:00Z"}"#;

        // Both enabled
        assert_eq!(
            parse_docker_log_line(stdout_line, true, true),
            Some("stdout msg".to_string())
        );
        assert_eq!(
            parse_docker_log_line(stderr_line, true, true),
            Some("stderr msg".to_string())
        );
    }

    #[test]
    fn test_parse_docker_log_invalid_json() {
        let invalid = "not json";
        assert_eq!(parse_docker_log_line(invalid, true, true), None);

        let incomplete = r#"{"log":"test"}"#; // Missing stream field
        assert_eq!(parse_docker_log_line(incomplete, true, true), None);
    }

    #[test]
    fn test_parse_docker_log_special_characters() {
        // Test with escaped characters
        let line = r#"{"log":"line with \"quotes\" and \\backslash","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(
            result,
            Some(r#"line with "quotes" and \backslash"#.to_string())
        );
    }

    #[test]
    fn test_parse_docker_log_empty_content() {
        let line = r#"{"log":"","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn test_parse_docker_log_multiline_content() {
        // Docker typically escapes newlines in log content
        let line = r#"{"log":"line1\\nline2","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert!(result.is_some());
        // The escaped newline should be preserved
        assert!(result.unwrap().contains("\\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_shared_containerd_config_uses_k3s_cni_paths() {
        let config = super::linux::shared_containerd_config();
        assert!(config.contains("bin_dir = \"/var/lib/rancher/k3s/data/cni\""));
        assert!(config.contains("conf_dir = \"/var/lib/rancher/k3s/agent/etc/cni/net.d\""));
        assert!(config.contains("max_conf_num = 1"));
    }

    // =========================================================================
    // Agent Creation Tests
    // =========================================================================

    #[test]
    fn test_agent_creation() {
        let _agent = Agent::new();
    }

    // =========================================================================
    // EnsureRuntime State Machine Tests
    // =========================================================================

    use crate::agent::ensure_runtime::{
        self, RuntimeGuard, RuntimeState, STATUS_FAILED, STATUS_REUSED, STATUS_STARTED,
    };
    use arcbox_protocol::agent::RuntimeEnsureResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Helper: creates a successful RuntimeEnsureResponse.
    fn make_ready_response() -> RuntimeEnsureResponse {
        RuntimeEnsureResponse {
            ready: true,
            endpoint: "vsock:2375".to_string(),
            message: "docker socket ready".to_string(),
            status: STATUS_STARTED.to_string(),
        }
    }

    /// Helper: creates a failed RuntimeEnsureResponse.
    fn make_failed_response() -> RuntimeEnsureResponse {
        RuntimeEnsureResponse {
            ready: false,
            endpoint: String::new(),
            message: "docker socket missing".to_string(),
            status: STATUS_FAILED.to_string(),
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_first_call_started() {
        let guard = RuntimeGuard::new();
        let response =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!("probe should not be called when start_if_needed=true")
            })
            .await;

        assert!(response.ready);
        assert_eq!(response.status, STATUS_STARTED);
        assert_eq!(response.endpoint, "vsock:2375");
    }

    #[tokio::test]
    async fn test_ensure_runtime_second_call_reused() {
        let guard = RuntimeGuard::new();

        // First call: starts runtime.
        let r1 =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!()
            })
            .await;
        assert_eq!(r1.status, STATUS_STARTED);

        // Second call: should reuse.
        let r2 = ensure_runtime::ensure_runtime(
            &guard,
            true,
            async { panic!("start_fn should not be called for reuse") },
            async { unreachable!() },
        )
        .await;
        assert!(r2.ready);
        assert_eq!(r2.status, STATUS_REUSED);
    }

    #[tokio::test]
    async fn test_ensure_runtime_20_sequential_calls_no_error() {
        let guard = RuntimeGuard::new();

        for i in 0..20 {
            let response = ensure_runtime::ensure_runtime(
                &guard,
                true,
                async { make_ready_response() },
                async { unreachable!() },
            )
            .await;
            assert!(response.ready, "call {} should succeed", i);
            if i == 0 {
                assert_eq!(response.status, STATUS_STARTED);
            } else {
                assert_eq!(response.status, STATUS_REUSED);
            }
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_probe_only_no_start() {
        let guard = RuntimeGuard::new();

        let response = ensure_runtime::ensure_runtime(
            &guard,
            false,
            async { panic!("start_fn should not be called when start_if_needed=false") },
            async {
                RuntimeEnsureResponse {
                    ready: false,
                    endpoint: String::new(),
                    message: "docker not available".to_string(),
                    status: STATUS_FAILED.to_string(),
                }
            },
        )
        .await;

        assert!(!response.ready);
        assert_eq!(response.status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn test_ensure_runtime_failed_then_retry_succeeds() {
        let guard = RuntimeGuard::new();

        // First call: fails.
        let r1 =
            ensure_runtime::ensure_runtime(&guard, true, async { make_failed_response() }, async {
                unreachable!()
            })
            .await;
        assert!(!r1.ready);
        assert_eq!(r1.status, STATUS_FAILED);

        // Second call: retry, now succeeds.
        let r2 =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!()
            })
            .await;
        assert!(r2.ready);
        assert_eq!(r2.status, STATUS_STARTED);

        // Third call: reused.
        let r3 = ensure_runtime::ensure_runtime(
            &guard,
            true,
            async { panic!("should not start again") },
            async { unreachable!() },
        )
        .await;
        assert!(r3.ready);
        assert_eq!(r3.status, STATUS_REUSED);
    }

    #[tokio::test]
    async fn test_ensure_runtime_concurrent_5_callers_consistent() {
        let guard = Arc::new(RuntimeGuard::new());
        let start_count = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(5));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let guard = Arc::clone(&guard);
            let start_count = Arc::clone(&start_count);
            let barrier = Arc::clone(&barrier);

            handles.push(tokio::spawn(async move {
                // Synchronize all 5 tasks to start concurrently.
                barrier.wait().await;

                ensure_runtime::ensure_runtime(
                    &guard,
                    true,
                    async {
                        start_count.fetch_add(1, Ordering::SeqCst);
                        // Simulate some startup delay.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        make_ready_response()
                    },
                    async { unreachable!() },
                )
                .await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // All 5 should report ready.
        for (i, r) in results.iter().enumerate() {
            assert!(r.ready, "caller {} should see ready", i);
        }

        // Exactly 1 should have status "started", rest "reused".
        let started_count = results
            .iter()
            .filter(|r| r.status == STATUS_STARTED)
            .count();
        let reused_count = results.iter().filter(|r| r.status == STATUS_REUSED).count();
        assert_eq!(started_count, 1, "exactly one caller should be the driver");
        assert_eq!(reused_count, 4, "other callers should get reused");

        // start_fn should have been invoked exactly once.
        assert_eq!(start_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_ensure_runtime_concurrent_5_callers_failure_consistent() {
        let guard = Arc::new(RuntimeGuard::new());
        let barrier = Arc::new(tokio::sync::Barrier::new(5));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let guard = Arc::clone(&guard);
            let barrier = Arc::clone(&barrier);

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                ensure_runtime::ensure_runtime(
                    &guard,
                    true,
                    async {
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                        make_failed_response()
                    },
                    async { unreachable!() },
                )
                .await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // All 5 should report not ready.
        for (i, r) in results.iter().enumerate() {
            assert!(!r.ready, "caller {} should see not ready", i);
            assert_eq!(r.status, STATUS_FAILED, "caller {} should get failed", i);
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_no_lost_wakeup_when_driver_finishes_fast() {
        // Repeat to make the regression deterministic enough: this used to
        // hang intermittently when notify happened before waiter registered.
        for _ in 0..50 {
            let guard = Arc::new(RuntimeGuard::new());
            let entered_start_fn = Arc::new(tokio::sync::Notify::new());
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

            let guard_driver = Arc::clone(&guard);
            let entered_start_fn_driver = Arc::clone(&entered_start_fn);
            let driver = tokio::spawn(async move {
                ensure_runtime::ensure_runtime(
                    &guard_driver,
                    true,
                    async move {
                        entered_start_fn_driver.notify_waiters();
                        let _ = release_rx.await;
                        make_ready_response()
                    },
                    async { unreachable!() },
                )
                .await
            });

            // Ensure state has transitioned to Starting before spawning waiter.
            entered_start_fn.notified().await;

            let guard_waiter = Arc::clone(&guard);
            let waiter = tokio::spawn(async move {
                ensure_runtime::ensure_runtime(
                    &guard_waiter,
                    true,
                    async { panic!("waiter should never run start_fn") },
                    async { unreachable!() },
                )
                .await
            });

            // Give waiter a chance to enter wait path, then let driver finish.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            let _ = release_tx.send(());

            let driver_resp = tokio::time::timeout(std::time::Duration::from_millis(500), driver)
                .await
                .expect("driver timed out")
                .expect("driver task failed");
            let waiter_resp = tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
                .await
                .expect("waiter timed out")
                .expect("waiter task failed");

            assert!(driver_resp.ready);
            assert_eq!(driver_resp.status, STATUS_STARTED);
            assert!(waiter_resp.ready);
            assert_eq!(waiter_resp.status, STATUS_REUSED);
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_state_machine_transitions() {
        let guard = RuntimeGuard::new();

        // Initially NotStarted.
        {
            let state = guard.state.lock().await;
            assert!(matches!(&*state, RuntimeState::NotStarted));
        }

        // After successful ensure: Ready.
        let _ =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!()
            })
            .await;
        {
            let state = guard.state.lock().await;
            assert!(
                matches!(&*state, RuntimeState::Ready { .. }),
                "expected Ready, got {:?}",
                *state
            );
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_state_machine_failed_to_ready() {
        let guard = RuntimeGuard::new();

        // Fail first.
        let _ =
            ensure_runtime::ensure_runtime(&guard, true, async { make_failed_response() }, async {
                unreachable!()
            })
            .await;
        {
            let state = guard.state.lock().await;
            assert!(
                matches!(&*state, RuntimeState::Failed { .. }),
                "expected Failed, got {:?}",
                *state
            );
        }

        // Retry succeeds.
        let _ =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!()
            })
            .await;
        {
            let state = guard.state.lock().await;
            assert!(
                matches!(&*state, RuntimeState::Ready { .. }),
                "expected Ready after retry, got {:?}",
                *state
            );
        }
    }

    #[tokio::test]
    async fn test_ensure_runtime_probe_after_ready_returns_reused() {
        let guard = RuntimeGuard::new();

        // Start first.
        let _ =
            ensure_runtime::ensure_runtime(&guard, true, async { make_ready_response() }, async {
                unreachable!()
            })
            .await;

        // Probe (start_if_needed=false) should return reused immediately
        // from the cached state, without calling probe_fn.
        let r = ensure_runtime::ensure_runtime(
            &guard,
            false,
            async { panic!("start_fn should not be called") },
            async { panic!("probe_fn should not be called when state is Ready") },
        )
        .await;
        assert!(r.ready);
        assert_eq!(r.status, STATUS_REUSED);
    }
}
