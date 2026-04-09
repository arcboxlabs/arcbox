/// Default vsock port for ArcBox guest agent RPC.
pub const AGENT_PORT: u32 = 1024;

/// Guest Docker API vsock proxy port.
pub const DOCKER_API_VSOCK_PORT: u32 = 2375;

/// Guest Kubernetes API vsock proxy port.
pub const KUBERNETES_API_VSOCK_PORT: u32 = 16443;

/// Vsock port for TCP port forwarding proxy in the guest agent.
///
/// The host opens a vsock connection to this port for each forwarded TCP
/// connection. The guest agent accepts, reads a 6-byte header
/// `[target_ip: 4][target_port: 2 BE]`, connects to the target, and
/// relays bidirectionally.
pub const PORT_FORWARD_VSOCK_PORT: u32 = 1025;

/// Host localhost port for the ArcBox Kubernetes API proxy.
pub const KUBERNETES_API_HOST_PORT: u16 = 16443;

/// Guest localhost port for the Kubernetes API server.
pub const KUBERNETES_API_GUEST_PORT: u16 = 6443;
