//! Linux-specific network infrastructure.
//!
//! This module provides Linux-specific implementations for:
//!
//! - **Bridge**: Linux bridge creation and management
//! - **TAP**: TAP device creation and management
//! - **Netlink**: Netlink socket operations for network configuration
//! - **Firewall**: iptables/nftables rule management
//! - **AF_PACKET**: Raw L2 frame I/O on named interfaces
//!
//! All types in this module require `target_os = "linux"`.

pub mod af_packet;
pub mod bridge;
pub mod firewall;
pub mod netlink;
pub mod tap;

pub use af_packet::open_af_packet;
pub use bridge::{BridgeConfig, LinuxBridge};
pub use firewall::{DnatRule, FirewallBackend, LinuxFirewall, NatRule, NatType, Protocol};
pub use netlink::{LinkConfig, LinkType, NetlinkHandle, Route};
pub use tap::{LinuxTap, TapConfig};
