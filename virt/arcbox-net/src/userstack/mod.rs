//! Cross-platform userspace network stack.
//!
//! This module provides a complete L2 network datapath that operates in
//! userspace, handling ARP, DHCP, DNS, UDP, ICMP, and TCP without requiring
//! kernel networking infrastructure (bridge, iptables, ip_forward).
//!
//! The datapath reads and writes raw Ethernet frames via any file descriptor
//! that supports `read()`/`write()` with message-boundary semantics — for
//! example, a `SOCK_DGRAM` socketpair (macOS / VZ framework) or an
//! `AF_PACKET SOCK_RAW` socket bound to a TAP interface (Linux / Firecracker).
//!
//! # Architecture
//!
//! ```text
//! Guest FD (socketpair / AF_PACKET)
//!     ↕ L2 Ethernet frames
//! SmoltcpDevice (frame classification)
//!     ├─ ARP           → smoltcp Interface (automatic)
//!     ├─ TCP           → smoltcp Interface → TcpBridge
//!     ├─ UDP:67 (DHCP) → DhcpServer
//!     ├─ UDP:53 to gw  → DnsForwarder
//!     ├─ UDP (other)   → SocketProxy (UdpProxy)
//!     └─ ICMP          → SocketProxy (IcmpProxy)
//! ```

pub mod datapath_loop;
pub mod inbound_relay;
pub mod smoltcp_device;
pub mod socket_proxy;
pub mod tcp_bridge;
