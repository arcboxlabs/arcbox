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
