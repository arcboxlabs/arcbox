/// Default vsock port for ArcBox guest agent RPC.
pub const AGENT_PORT: u32 = 1024;

/// Guest Docker API vsock proxy port.
pub const DOCKER_API_VSOCK_PORT: u32 = 2375;

/// Guest Kubernetes API vsock proxy port.
pub const KUBERNETES_API_VSOCK_PORT: u32 = 16443;

/// Host localhost port for the ArcBox Kubernetes API proxy.
pub const KUBERNETES_API_HOST_PORT: u16 = 16443;

/// Host loopback port for the ArcBox SSH server (`ssh <machine>@arcbox`).
/// ArcBox's 16xxx family beside the Kubernetes proxy, and clear of the
/// 32222 OrbStack uses so both can run side by side.
pub const SSH_HOST_PORT: u16 = 16022;

/// Host loopback UDP port of the production daemon's DNS server, the port
/// `/etc/resolver/arcbox.local` names.
pub const DNS_HOST_PORT: u16 = 5553;

/// Host loopback UDP port of the development daemon's DNS server.
///
/// Distinct so a development daemon runs beside the production one; the
/// development profile never owns the canonical resolver file, so nothing
/// else on the host has to know this number.
pub const DEVELOPMENT_DNS_HOST_PORT: u16 = 5554;

/// Guest localhost port for the Kubernetes API server.
pub const KUBERNETES_API_GUEST_PORT: u16 = 6443;

/// Guest vsock port relaying to the in-guest kernel nfsd (NFS protocol).
///
/// The host daemon bridges a localhost TCP proxy to this port; the guest relay
/// forwards it to `127.0.0.1:2049`. NFSv4 serves everything on this one port,
/// so no separate MOUNT-protocol relay is needed.
pub const NFS_NFSD_RELAY_PORT: u32 = 2049;

/// Guest vsock port for the SSH-auth agent-forwarding relay.
///
/// Unlike the other relays, the host is the party that must reach a resource
/// on its own side (the user's `ssh-agent`), while the connection originates
/// in the guest (a container talks to the forwarded socket). VZ exposes only
/// host→guest dialing, so the daemon instead pre-dials a pool of connections
/// to this port and parks them; the guest agent hands each parked connection
/// to the next container that opens the forwarded socket. See
/// `arcbox_constants::paths::HOST_SERVICES_SSH_AUTH_SOCK`.
pub const SSH_AUTH_RELAY_PORT: u32 = 2201;
