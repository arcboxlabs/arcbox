//! Lightweight DHCP server and IP allocator for ArcBox.
//!
//! This crate provides a self-contained DHCP server implementation with:
//!
//! - DHCP DISCOVER/OFFER/REQUEST/ACK flow
//! - IP address pool allocation and lease management
//! - DNS server and gateway configuration
//! - MAC-based IP reservations
//!
//! # Example
//!
//! ```no_run
//! use arcbox_dhcp::{DhcpServer, DhcpConfig};
//! use std::net::Ipv4Addr;
//!
//! let config = DhcpConfig::new(
//!     Ipv4Addr::new(192, 168, 64, 1),
//!     Ipv4Addr::new(255, 255, 255, 0),
//! );
//!
//! let mut server = DhcpServer::new(config);
//! ```

pub mod allocator;
pub mod config;
pub mod error;
pub mod packet;
pub mod server;

pub use allocator::IpAllocator;
pub use config::{DEFAULT_LEASE_DURATION, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpConfig};
pub use error::{DhcpError, Result};
pub use packet::{DhcpMessageType, DhcpPacket};
pub use server::{DhcpLease, DhcpServer};
