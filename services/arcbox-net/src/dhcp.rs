//! DHCP server — re-exported from [`arcbox_dhcp`].

// Re-export types but NOT `Result`/`DhcpError` to avoid shadowing
// `arcbox_net::Result` / `arcbox_net::NetError` at the `arcbox_net::dhcp::*` level.
pub use arcbox_dhcp::{
    DhcpConfig, DhcpLease, DhcpMessageType, DhcpPacket, DhcpServer, IpAllocator,
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DEFAULT_LEASE_DURATION,
};

// Error types are available via their fully-qualified path
// (`arcbox_dhcp::DhcpError`) when needed.
pub use arcbox_dhcp::DhcpError;
