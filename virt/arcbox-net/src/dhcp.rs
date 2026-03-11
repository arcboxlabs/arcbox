//! DHCP server — re-exported from [`arcbox_dhcp`].

// Re-export core types but NOT the `Result` alias to avoid shadowing
// `arcbox_net::Result` / `arcbox_net::NetError` at the `arcbox_net::dhcp::*` level.
pub use arcbox_dhcp::{
    DEFAULT_LEASE_DURATION, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpConfig, DhcpError, DhcpLease,
    DhcpMessageType, DhcpPacket, DhcpServer, IpAllocator,
};
