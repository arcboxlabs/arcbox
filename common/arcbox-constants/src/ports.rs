/// Default vsock port for ArcBox guest agent RPC.
pub const AGENT_PORT: u32 = 1024;

/// Guest Docker API vsock proxy port.
pub const DOCKER_API_VSOCK_PORT: u32 = 2375;

/// Guest Kubernetes API vsock proxy port.
pub const KUBERNETES_API_VSOCK_PORT: u32 = 16443;

/// Host localhost port for the ArcBox Kubernetes API proxy.
pub const KUBERNETES_API_HOST_PORT: u16 = 16443;

/// Guest localhost port for the Kubernetes API server.
pub const KUBERNETES_API_GUEST_PORT: u16 = 6443;
