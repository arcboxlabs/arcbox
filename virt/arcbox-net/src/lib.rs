//! # arcbox-net
//!
//! Network configuration, DNS, and platform packet I/O for ArcBox.
//!
//! [`NetworkManager`] holds network configuration, IP allocation, and local
//! DNS names. The VMM creates VM network attachments and their packet paths.
//!
//! ## Architecture
//!
//! ```text
//! NetworkManager
//!     ├─ nat::IpAllocator
//!     └─ dns::DnsForwarder and shared local DNS names
//!
//! macOS primary NIC (VZ or HV)
//!     → darwin::datapath_loop::NetworkDatapath
//!         ├─ ARP, DHCP, DNS replies
//!         └─ TCP bridge, UDP/ICMP proxies → host sockets
//! ```
//!
//! The macOS primary NIC uses the socket proxy datapath above. A separate
//! bridge NIC uses `arcbox-vmnet`. Linux TAP and bridge implementations live
//! in the `linux` module. These are distinct platform paths.
//!
//! [`backend::NetworkBackend`] provides a packet I/O interface for the TAP
//! and vmnet adapters. [`nat_engine`] and [`datapath`] re-export shared
//! connection-tracking and packet-processing primitives.

#![allow(unused_imports, unused_variables, unused_mut)]

pub mod backend;
pub mod datapath;
pub mod dhcp;
// `direct_rx` (FrameSink/ConnSink/PromotedConn) is extracted to
// `splicetcp`; re-exported so the `arcbox_net::direct_rx` path is
// unchanged for the VMM layer and the inject crate.
pub use splicetcp::direct_rx;
pub mod dns;
mod dns_projection;
pub mod error;
// `ethernet` is extracted to `arcbox-packet`; re-exported here so the
// `arcbox_net::ethernet` path is unchanged for callers.
pub use arcbox_packet::ethernet;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod nat;
pub mod nat_engine;
pub mod port_forward;
pub mod timer_wheel;

#[cfg(target_os = "macos")]
pub mod darwin;

pub use error::{NetError, Result};

/// Network mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// NAT networking (default).
    #[default]
    Nat,
    /// Bridge networking.
    Bridge,
    /// Host-only networking.
    HostOnly,
    /// No networking.
    None,
}

/// Network configuration.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// DNS configuration fixed when the network manager is constructed.
    pub dns: dns::DnsConfig,
    /// Network mode.
    pub mode: NetworkMode,
    /// MAC address (auto-generated if None).
    pub mac: Option<[u8; 6]>,
    /// MTU size.
    pub mtu: u16,
    /// Bridge interface name (for bridge mode).
    pub bridge: Option<String>,
    /// Enable multiqueue.
    pub multiqueue: bool,
    /// Number of queues.
    pub num_queues: u32,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            dns: dns::DnsConfig::default(),
            mode: NetworkMode::Nat,
            mac: None,
            mtu: 1500,
            bridge: None,
            multiqueue: false,
            num_queues: 1,
        }
    }
}

use std::net::IpAddr;
use std::sync::RwLock;

use tokio::sync::{broadcast, watch};

/// Network manager state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    /// Network manager not started.
    Stopped,
    /// Network manager is starting.
    Starting,
    /// Network manager is running.
    Running,
    /// Network manager is stopping.
    Stopping,
}

/// A change to the names served under the local DNS domain.
///
/// Emitted by [`NetworkManager::replace_dns_projection`] for the daemon's mDNS mirror.
/// `fqdn` is the canonical full name published by the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsChange {
    /// `fqdn` now resolves to `ip` (also sent when the address changes).
    Registered { fqdn: String, ip: IpAddr },
    /// `fqdn` no longer resolves.
    Removed { fqdn: String },
}

/// Changes a slow subscriber may fall behind by before it must resync from
/// [`NetworkManager::local_dns_entries`].
const DNS_CHANGE_CAPACITY: usize = 256;

/// Network manager.
///
/// Manages the network infrastructure for VMs including:
/// - Network backends (vmnet on macOS, TAP/bridge on Linux)
/// - NAT engine for address translation
/// - Port forwarding
/// - DNS forwarding
pub struct NetworkManager {
    config: NetConfig,
    /// Current state.
    state: RwLock<NetworkState>,
    /// IP allocator for NAT.
    ip_allocator: RwLock<Option<nat::IpAllocator>>,
    /// DNS forwarder with local hostname resolution.
    dns_forwarder: dns::DnsForwarder,
    /// Local-domain name changes, for mirrors of the DNS table.
    dns_changes: broadcast::Sender<DnsChange>,
    /// Complete ownership changes, including claims without concrete records.
    dns_registry_epoch: watch::Sender<u64>,
}

impl NetworkManager {
    /// Creates a new network manager.
    #[must_use]
    pub fn new(config: NetConfig) -> Self {
        let dns_forwarder = dns::DnsForwarder::new(config.dns.clone());
        Self {
            config,
            state: RwLock::new(NetworkState::Stopped),
            ip_allocator: RwLock::new(None),
            dns_forwarder,
            dns_changes: broadcast::channel(DNS_CHANGE_CAPACITY).0,
            dns_registry_epoch: watch::channel(0).0,
        }
    }

    /// The local domain fixed by [`NetConfig::dns`] at construction.
    /// `None` means unmatched names can be forwarded upstream without a reserved suffix.
    #[must_use]
    pub fn dns_domain(&self) -> Option<String> {
        self.dns_forwarder.config().local_domain.clone()
    }

    /// Returns the network configuration.
    #[must_use]
    pub fn config(&self) -> &NetConfig {
        &self.config
    }

    /// Returns the current state.
    #[must_use]
    pub fn state(&self) -> NetworkState {
        *self.state.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Starts the network manager.
    ///
    /// This initializes the network backend based on the configured mode:
    /// - NAT: Creates NAT network with IP allocation and port forwarding
    /// - Bridge: Attaches to existing bridge interface
    /// - HostOnly: Creates isolated network
    /// - None: No network initialization
    ///
    /// # Errors
    ///
    /// Returns an error if the network cannot be started.
    pub fn start(&self) -> Result<()> {
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| NetError::config("lock poisoned".to_string()))?;

            if *state != NetworkState::Stopped {
                return Ok(());
            }
            *state = NetworkState::Starting;
        }

        let result = self.do_start();

        {
            let mut state = self
                .state
                .write()
                .map_err(|_| NetError::config("lock poisoned".to_string()))?;
            *state = if result.is_ok() {
                NetworkState::Running
            } else {
                NetworkState::Stopped
            };
        }

        result
    }

    /// Internal start implementation.
    fn do_start(&self) -> Result<()> {
        match self.config.mode {
            NetworkMode::Nat => {
                // Initialize IP allocator for NAT mode.
                let allocator = nat::IpAllocator::new(
                    std::net::Ipv4Addr::new(192, 168, 64, 2),
                    std::net::Ipv4Addr::new(192, 168, 64, 254),
                );

                let mut ip_alloc = self
                    .ip_allocator
                    .write()
                    .map_err(|_| NetError::config("lock poisoned".to_string()))?;
                *ip_alloc = Some(allocator);

                tracing::info!("Network manager started in NAT mode");
            }
            NetworkMode::Bridge => {
                if self.config.bridge.is_none() {
                    return Err(NetError::config(
                        "bridge mode requires bridge interface name".to_string(),
                    ));
                }
                tracing::info!(
                    "Network manager started in bridge mode ({})",
                    self.config.bridge.as_deref().unwrap_or("unknown")
                );
            }
            NetworkMode::HostOnly => {
                // Initialize IP allocator for host-only mode.
                let allocator = nat::IpAllocator::new(
                    std::net::Ipv4Addr::new(10, 0, 0, 2),
                    std::net::Ipv4Addr::new(10, 0, 0, 254),
                );

                let mut ip_alloc = self
                    .ip_allocator
                    .write()
                    .map_err(|_| NetError::config("lock poisoned".to_string()))?;
                *ip_alloc = Some(allocator);

                tracing::info!("Network manager started in host-only mode");
            }
            NetworkMode::None => {
                tracing::info!("Network manager started with no networking");
            }
        }

        Ok(())
    }

    /// Stops the network manager.
    ///
    /// Current stop behavior:
    /// - Clears the in-memory IP allocator state
    /// - Transitions manager state back to `Stopped`
    ///
    /// # Errors
    ///
    /// Returns an error if the network cannot be stopped.
    pub fn stop(&self) -> Result<()> {
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| NetError::config("lock poisoned".to_string()))?;

            if *state != NetworkState::Running {
                return Ok(());
            }
            *state = NetworkState::Stopping;
        }

        let result = self.do_stop();

        {
            let mut state = self
                .state
                .write()
                .map_err(|_| NetError::config("lock poisoned".to_string()))?;
            *state = NetworkState::Stopped;
        }

        result
    }

    /// Internal stop implementation.
    #[allow(clippy::unnecessary_wraps)] // Result kept for forward-compatibility (stop may fail in future)
    fn do_stop(&self) -> Result<()> {
        // Clear IP allocator.
        if let Ok(mut ip_alloc) = self.ip_allocator.write() {
            *ip_alloc = None;
        }

        tracing::info!("Network manager stopped");
        Ok(())
    }

    /// Allocates an IP address from the pool.
    ///
    /// Returns None if no addresses are available or network is not started.
    pub fn allocate_ip(&self) -> Option<std::net::Ipv4Addr> {
        let mut ip_alloc = self.ip_allocator.write().ok()?;
        ip_alloc.as_mut()?.allocate()
    }

    /// Releases an IP address back to the pool.
    pub fn release_ip(&self, ip: std::net::Ipv4Addr) {
        if let Ok(mut ip_alloc) = self.ip_allocator.write()
            && let Some(allocator) = ip_alloc.as_mut()
        {
            allocator.release(ip);
        }
    }

    /// Returns the number of available IP addresses.
    #[must_use]
    pub fn available_ips(&self) -> usize {
        self.ip_allocator
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(nat::IpAllocator::available_count))
            .unwrap_or(0)
    }

    /// Returns the concrete DNS projection used for mDNS publication and snapshots.
    pub fn local_hosts_table(&self) -> std::sync::Arc<arcbox_dns::LocalHostsTable> {
        self.dns_forwarder.local_hosts_table()
    }

    /// Shares DNS ownership with Runtime and every VM datapath.
    pub fn dns_registry(
        &self,
    ) -> std::sync::Arc<std::sync::RwLock<arcbox_dns::registry::Registry>> {
        self.dns_forwarder.registry()
    }

    /// Shares DNS ownership and the configured relative-name suffix with VM datapaths.
    pub fn shared_dns(&self) -> dns::SharedDns {
        dns::SharedDns {
            registry: self.dns_registry(),
            local_domain: self.dns_forwarder.config().local_domain.clone(),
        }
    }

    /// Streams every later change to the names under the local domain.
    ///
    /// Subscribe first, then read [`Self::local_dns_entries`], so no change
    /// falls between the two; on `Lagged` read the entries again.
    #[must_use]
    pub fn subscribe_dns_changes(&self) -> broadcast::Receiver<DnsChange> {
        self.dns_changes.subscribe()
    }

    /// Watches the latest ownership epoch, including changes without concrete records.
    /// Subscribe before reading the Registry snapshot. Drop watch borrows before
    /// acquiring the Registry lock; the publisher holds that lock during notification.
    #[must_use]
    pub fn subscribe_dns_registry(&self) -> watch::Receiver<u64> {
        self.dns_registry_epoch.subscribe()
    }

    /// Every `(fqdn, ip)` currently served under the local domain.
    #[must_use]
    pub fn local_dns_entries(&self) -> Vec<(String, IpAddr)> {
        self.dns_forwarder.local_domain_entries()
    }

    /// Tries to resolve a DNS query locally, answering NODATA for unresolved
    /// names under the local domain (see
    /// `DnsForwarder::try_resolve_locally_or_nodata` for why not NXDOMAIN).
    /// Returns `None` only when the query should be forwarded to upstream DNS.
    pub fn try_resolve_dns_or_nodata(&self, query: &[u8]) -> Option<Vec<u8>> {
        self.dns_forwarder.try_resolve_locally_or_nodata(query)
    }

    /// Handles a full DNS query: local resolution first, then upstream forwarding.
    /// This blocks on network I/O for upstream queries.
    pub fn handle_dns_query(&self, query: &[u8]) -> Result<Vec<u8>> {
        self.dns_forwarder.handle_query(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(manager: &NetworkManager, ip: IpAddr) {
        let claims = arcbox_dns::claims::Claim::generated(
            "web.custom.test".parse().unwrap(),
            "web".into(),
            "test".into(),
        );
        let registry = manager.dns_registry();
        let mut registry = registry.write().unwrap();
        registry
            .replace("web".into(), claims.into(), Some(ip))
            .unwrap();
        manager.replace_dns_projection(&registry);
    }

    #[test]
    fn dns_changes_name_the_fqdn_from_construction_config() {
        let manager = NetworkManager::new(NetConfig {
            dns: dns::DnsConfig::default().with_local_domain("custom.test"),
            ..Default::default()
        });
        let mut changes = manager.subscribe_dns_changes();
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 2, 5));

        register(&manager, ip);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: "web.custom.test".into(),
                ip
            }
        );
        assert_eq!(
            manager.local_dns_entries(),
            vec![("web.custom.test".to_owned(), ip)],
            "only the FQDN entry is listed, not the bare hostname"
        );

        {
            let registry = manager.dns_registry();
            let mut registry = registry.write().unwrap();
            registry.remove("web");
            manager.replace_dns_projection(&registry);
        }
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Removed {
                fqdn: "web.custom.test".into()
            }
        );

        assert_eq!(manager.local_dns_entries(), [] as [(String, IpAddr); 0]);
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn test_network_manager_lifecycle() {
        let manager = NetworkManager::new(NetConfig::default());
        assert_eq!(manager.state(), NetworkState::Stopped);

        manager.start().unwrap();
        assert_eq!(manager.state(), NetworkState::Running);

        // Can allocate IPs in NAT mode.
        let ip = manager.allocate_ip();
        assert!(ip.is_some());

        manager.stop().unwrap();
        assert_eq!(manager.state(), NetworkState::Stopped);
    }

    #[test]
    fn test_network_manager_ip_allocation() {
        let manager = NetworkManager::new(NetConfig::default());
        manager.start().unwrap();

        let ip1 = manager.allocate_ip().unwrap();
        let ip2 = manager.allocate_ip().unwrap();
        assert_ne!(ip1, ip2);

        // Release ip1 and verify it can be allocated again (eventually).
        manager.release_ip(ip1);
        // Allocator uses sequential allocation, so ip3 may not be ip1 immediately.
        // But after release, the pool should have one more available slot.
        let initial_available = manager.available_ips();
        let _ip3 = manager.allocate_ip().unwrap();
        // Available count should decrease after allocation.
        assert!(manager.available_ips() < initial_available || initial_available == 253);
    }

    #[test]
    fn network_manager_uses_its_construction_domain() {
        let manager = NetworkManager::new(NetConfig {
            dns: dns::DnsConfig::default().with_local_domain("custom.test"),
            ..Default::default()
        });
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(172, 17, 0, 2));
        register(&manager, ip);

        // Helper to build a minimal A-record query.
        let build_query = |name: &str| -> Vec<u8> {
            let mut pkt = vec![0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00];
            pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
            for label in name.split('.') {
                pkt.push(label.len() as u8);
                pkt.extend_from_slice(label.as_bytes());
            }
            pkt.push(0x00);
            pkt.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
            pkt
        };

        let q = build_query("web.custom.test");
        let resp = manager.try_resolve_dns_or_nodata(&q).unwrap();
        assert_eq!(resp[3] & 0x0F, 0, "should resolve under custom domain");
        assert_eq!(resp[7], 1, "ANCOUNT=1");

        // Unknown under custom domain → NODATA.
        let q = build_query("nope.custom.test");
        let resp = manager.try_resolve_dns_or_nodata(&q).unwrap();
        assert_eq!(
            resp[3] & 0x0F,
            0,
            "NOERROR for unregistered custom-domain host"
        );
        assert_eq!(resp[7], 0, "no answer for unregistered custom-domain host");

        // The default domain is outside this manager's configured scope.
        let q = build_query("web.arcbox.local");
        assert!(
            manager.try_resolve_dns_or_nodata(&q).is_none(),
            "the default domain must not match a different configured suffix"
        );
    }

    #[test]
    fn test_network_manager_no_network_mode() {
        let config = NetConfig {
            mode: NetworkMode::None,
            ..Default::default()
        };
        let manager = NetworkManager::new(config);
        manager.start().unwrap();

        // No IP allocation in none mode.
        assert!(manager.allocate_ip().is_none());
    }
}
