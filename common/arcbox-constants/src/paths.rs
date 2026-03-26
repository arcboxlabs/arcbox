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
