/// Guest mount point for dockerd persistent state (`/var/lib/docker`).
///
/// Backed by the Btrfs `@docker` subvolume on `/dev/vdb`.
pub const DOCKER_DATA_MOUNT_POINT: &str = "/var/lib/docker";

/// Guest mount point for containerd persistent state (`/var/lib/containerd`).
///
/// Backed by the Btrfs `@containerd` subvolume on `/dev/vdb`.
pub const CONTAINERD_DATA_MOUNT_POINT: &str = "/var/lib/containerd";

/// Docker Engine API Unix socket path in guest.
pub const DOCKER_API_UNIX_SOCKET: &str = "/var/run/docker.sock";

/// containerd gRPC socket path in guest.
pub const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";

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
}

/// Guest-side subdirectory names within `/arcbox/`.
pub mod guest {
    /// Log directory inside the VirtioFS mount.
    pub const LOG: &str = "log";
}
