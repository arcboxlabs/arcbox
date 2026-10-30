//! The guest's primary NAT network, as both macOS VMMs build it.
//!
//! The VMM owns the socketpair and its VMM-side end; this module builds
//! everything behind the host end: DHCP, the DNS forwarder, host egress,
//! the egress proxy, and the inbound listener manager for port forwarding.

use std::net::Ipv4Addr;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::datapath_loop::NetworkDatapath;
use super::egress::HostEgress;
use super::inbound_relay::InboundListenerManager;
use super::proxy_detect::ProxyEnvironment;
use crate::dhcp::{DhcpConfig, DhcpServer};
use crate::dns::{DnsConfig, DnsForwarder, SharedDns};

/// The gateway address the guest sees.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);
/// The address DHCP leases to the guest.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The gateway's MAC address.
pub const GATEWAY_MAC: [u8; 6] = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const POOL_END: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 254);

/// A built, not yet running, guest NAT datapath.
pub struct GuestNat {
    /// The datapath. The caller attaches any VMM-specific sinks, then
    /// spawns [`NetworkDatapath::run`].
    pub datapath: NetworkDatapath,
    /// Port forwarding into the guest, fed through the datapath.
    pub inbound: InboundListenerManager,
    /// Stops every task the datapath spawns.
    pub cancel: CancellationToken,
}

impl GuestNat {
    /// Builds the datapath behind `host_fd`, the host end of the guest
    /// NIC's socketpair.
    ///
    /// `mtu` is the MTU the guest NIC negotiates. `dns` shares name
    /// ownership with the host DNS service; without it the forwarder keeps
    /// its own registry. `proxy` tunnels guest egress; without it guest
    /// flows connect directly from the host.
    pub fn new(
        host_fd: OwnedFd,
        mtu: usize,
        dns: Option<&SharedDns>,
        proxy: Option<ProxyEnvironment>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let (reply_tx, reply_rx) = mpsc::channel::<Vec<u8>>(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let egress = HostEgress::new(
            GATEWAY_IP,
            GATEWAY_MAC,
            GUEST_IP,
            reply_tx,
            cancel.clone(),
            mtu,
        );
        let inbound = InboundListenerManager::new(cmd_tx);

        let dhcp_config = DhcpConfig::new(GATEWAY_IP, NETMASK)
            .with_pool_range(GUEST_IP, POOL_END)
            .with_dns_servers(vec![GATEWAY_IP]);
        let mut dns_config = DnsConfig::new(GATEWAY_IP);
        let dns_forwarder = match dns {
            Some(dns) => {
                dns_config.local_domain.clone_from(&dns.local_domain);
                DnsForwarder::with_shared_registry(dns_config, Arc::clone(&dns.registry))
            }
            None => DnsForwarder::new(dns_config),
        };

        let mut datapath = NetworkDatapath::new(
            host_fd,
            egress,
            reply_rx,
            cmd_rx,
            DhcpServer::new(dhcp_config),
            dns_forwarder,
            GATEWAY_IP,
            GUEST_IP,
            GATEWAY_MAC,
            cancel.clone(),
            mtu,
        );
        if let Some(proxy) = proxy {
            datapath.set_proxy_env(proxy);
        }
        Self {
            datapath,
            inbound,
            cancel,
        }
    }
}
