/// Guest mount point for dockerd persistent state (`/var/lib/docker`).
///
/// Backed by the Btrfs `@docker` subvolume on `/dev/vdb`.
pub const DOCKER_DATA_MOUNT_POINT: &str = "/var/lib/docker";

/// Guest mount point for containerd persistent state (`/var/lib/containerd`).
///
/// Backed by the Btrfs `@containerd` subvolume on `/dev/vdb`.
pub const CONTAINERD_DATA_MOUNT_POINT: &str = "/var/lib/containerd";

/// Guest mount point for K3s persistent state (`/var/lib/rancher/k3s`).
pub const K3S_DATA_MOUNT_POINT: &str = "/var/lib/rancher/k3s";

/// Guest mount point for kubelet persistent state (`/var/lib/kubelet`).
pub const KUBELET_DATA_MOUNT_POINT: &str = "/var/lib/kubelet";

/// Guest mount point for CNI persistent state (`/var/lib/cni`).
pub const CNI_DATA_MOUNT_POINT: &str = "/var/lib/cni";

/// Docker Engine API Unix socket path in guest.
pub const DOCKER_API_UNIX_SOCKET: &str = "/var/run/docker.sock";

/// containerd gRPC socket path in guest.
pub const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";

/// K3s-generated kubeconfig path inside the guest.
pub const K3S_KUBECONFIG_PATH: &str = "/var/lib/rancher/k3s/k3s.yaml";

/// K3s-managed CNI config directory for the kubelet/containerd stack.
pub const K3S_CNI_CONF_DIR: &str = "/var/lib/rancher/k3s/agent/etc/cni/net.d";

/// K3s-managed CNI plugin directory used by current releases.
pub const K3S_CNI_BIN_DIR: &str = "/var/lib/rancher/k3s/data/cni";

/// Directory where runtime binaries (containerd, dockerd, runc, …) are
/// accessed via VirtioFS live execution.
pub const ARCBOX_RUNTIME_BIN_DIR: &str = "/arcbox/runtime/bin";

/// Host-side privileged paths (require root to write).
pub mod privileged {
    /// Installed helper binary path.
    pub const HELPER_BINARY: &str = "/usr/local/libexec/arcbox-helper";
    /// Helper launchd plist path.
    pub const HELPER_PLIST: &str = "/Library/LaunchDaemons/com.arcboxlabs.desktop.helper.plist";
    /// Helper socket-activation socket path.
    pub const HELPER_SOCKET: &str = "/var/run/arcbox-helper.sock";
    /// Docker socket symlink path.
    pub const DOCKER_SOCKET: &str = "/var/run/docker.sock";
}

/// launchd service labels.
pub mod labels {
    /// Daemon (user-level LaunchAgent).
    pub const DAEMON: &str = "com.arcboxlabs.desktop.daemon";
    /// Helper (system-level LaunchDaemon).
    pub const HELPER: &str = "com.arcboxlabs.desktop.helper";
}

/// Docker CLI tool names managed by the helper's cli_link.
pub const DOCKER_CLI_TOOLS: &[&str] = &[
    "docker",
    "docker-buildx",
    "docker-compose",
    "docker-credential-osxkeychain",
];

/// Host-side subdirectory names within `~/.arcbox/`.
pub mod host {
    /// Runtime state (sockets, PID files, ephemeral markers).
    pub const RUN: &str = "run";
    /// Centralized log directory.
    pub const LOG: &str = "log";
    /// Persistent data aggregation (images, containers, volumes, …).
    pub const DATA: &str = "data";

    /// Log file names (written by each component's tracing-appender).
    pub const DAEMON_LOG: &str = "daemon.log";
    pub const AGENT_LOG: &str = "agent.log";

    /// Default daemon lock file name (inside `RUN`).
    pub const DAEMON_LOCK: &str = "daemon.lock";
    /// Default Docker API socket name (inside `RUN`).
    pub const DOCKER_SOCKET: &str = "docker.sock";
    /// Default gRPC API socket name (inside `RUN`).
    pub const GRPC_SOCKET: &str = "arcbox.sock";
}

/// Resolved host-side directory layout.
///
/// Single source of truth for every path derived from the ArcBox data
/// directory (`~/.arcbox` by default). Both the daemon and CLI should
/// construct a `HostLayout` once and pass it around instead of
/// recalculating paths independently.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct HostLayout {
    /// Root data directory (e.g. `~/.arcbox`).
    pub data_dir: std::path::PathBuf,
    /// `<data_dir>/run` — sockets, PID file, lock.
    pub run_dir: std::path::PathBuf,
    /// `<data_dir>/log` — daemon and agent logs.
    pub log_dir: std::path::PathBuf,
    /// `<data_dir>/data` — persistent VM/container data.
    pub data_subdir: std::path::PathBuf,
    /// `<run_dir>/docker.sock`
    pub docker_socket: std::path::PathBuf,
    /// `<run_dir>/arcbox.sock`
    pub grpc_socket: std::path::PathBuf,
    /// `<run_dir>/daemon.lock`
    pub lock_file: std::path::PathBuf,
    /// `<log_dir>/daemon.log`
    pub daemon_log: std::path::PathBuf,
}

#[cfg(feature = "std")]
impl HostLayout {
    /// Build a layout from an explicit data directory.
    #[must_use]
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        let run_dir = data_dir.join(host::RUN);
        let log_dir = data_dir.join(host::LOG);
        let data_subdir = data_dir.join(host::DATA);
        let docker_socket = run_dir.join(host::DOCKER_SOCKET);
        let grpc_socket = run_dir.join(host::GRPC_SOCKET);
        let lock_file = run_dir.join(host::DAEMON_LOCK);
        let daemon_log = log_dir.join(host::DAEMON_LOG);
        Self {
            data_dir,
            run_dir,
            log_dir,
            data_subdir,
            docker_socket,
            grpc_socket,
            lock_file,
            daemon_log,
        }
    }

    /// Resolve the data directory from an optional override, falling
    /// back to `~/.arcbox` (or `/var/lib/arcbox` when `$HOME` is unset).
    #[must_use]
    pub fn resolve(data_dir: Option<&std::path::Path>) -> Self {
        let data_dir = match data_dir {
            Some(d) => d.to_path_buf(),
            None => default_data_dir(),
        };
        Self::new(data_dir)
    }
}

/// Default data directory: `$HOME/.arcbox`, or `/var/lib/arcbox` when
/// `$HOME` is not set.
#[cfg(feature = "std")]
#[must_use]
pub fn default_data_dir() -> std::path::PathBuf {
    std::env::var_os("HOME").map_or_else(
        || std::path::PathBuf::from("/var/lib/arcbox"),
        |home| std::path::PathBuf::from(home).join(".arcbox"),
    )
}

/// Privileged log directory (root-owned, for arcbox-helper).
pub mod privileged_log {
    /// Directory for helper logs (root-writable).
    pub const HELPER_LOG_DIR: &str = "/var/log/arcbox";
    /// Helper log file name.
    pub const HELPER_LOG: &str = "helper.log";
}

/// Guest-side subdirectory names within `/arcbox/`.
pub mod guest {
    /// Log directory inside the VirtioFS mount.
    pub const LOG: &str = "log";
}
