//! Host-socket egress for guest network traffic.
//!
//! Instead of forwarding raw IP packets through a utun and relying on kernel
//! routing/NAT, each outbound packet is parsed and driven onto host OS sockets
//! directly. This bypasses kernel routing, VPN interference, and pf issues.
//!
//! # Architecture
//!
//! ```text
//! Guest frame → HostEgress::handle_outbound()
//!   ├─ ICMP → icmp::IcmpProxy (ICMP datagram socket sendto/recv)
//!   └─ UDP  → udp::UdpProxy   (per-flow host UdpSocket, optionally via SOCKS5)
//!         ↓
//!   reply_tx → mpsc → datapath select! → guest FD
//! ```

mod icmp;
mod udp;

use std::net::Ipv4Addr;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use arcbox_fakeip::dns_log::DnsResolutionLog;
use arcbox_fakeip::proxy_detect::ProxyEnvironment;
use arcbox_packet::ethernet::ETH_HEADER_LEN;

use icmp::IcmpProxy;
use udp::UdpProxy;

/// Top-level host-socket egress that dispatches guest traffic to protocol-specific
/// handlers and manages inbound port forwarding.
pub struct HostEgress {
    icmp: IcmpProxy,
    udp: UdpProxy,
    /// Inbound relay for host → guest port forwarding.
    inbound: super::inbound_relay::InboundRelay,
    /// Shared reply sender for injecting L2 frames towards the guest.
    reply_tx: mpsc::Sender<Vec<u8>>,
    /// Cancellation token for graceful shutdown of spawned tasks.
    cancel: CancellationToken,
}

impl HostEgress {
    /// Creates a new host-socket egress.
    ///
    /// `reply_tx` is used by all sub-proxies to send L2 frames back to the
    /// datapath for writing to the guest FD. `mtu` is the guest link MTU;
    /// guest-bound UDP datagrams above it are IPv4-fragmented.
    #[must_use]
    pub fn new(
        gateway_ip: Ipv4Addr,
        gateway_mac: [u8; 6],
        guest_ip: Ipv4Addr,
        reply_tx: mpsc::Sender<Vec<u8>>,
        cancel: CancellationToken,
        mtu: usize,
    ) -> Self {
        let inbound = super::inbound_relay::InboundRelay::new(
            reply_tx.clone(),
            gateway_mac,
            gateway_ip,
            guest_ip,
            mtu,
        );
        Self {
            icmp: IcmpProxy::new(reply_tx.clone(), gateway_mac),
            udp: UdpProxy::new(reply_tx.clone(), gateway_mac, gateway_ip, mtu),
            inbound,
            reply_tx,
            cancel,
        }
    }

    /// Returns a clone of the reply sender for external use (e.g. async DNS
    /// forwarding).
    #[must_use]
    pub fn reply_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.reply_tx.clone()
    }

    /// Makes the UDP path proxy-aware, mirroring `TcpBridge::set_proxy_awareness`:
    /// shares the fake-IP `dns_log` (so a fake-IP destination is reversed to its
    /// domain) and the detected `proxy_env` (so a configured SOCKS proxy routes
    /// non-gateway, non-bypassed UDP flows). Opt-in and off by default, so the VMM
    /// and existing callers are unaffected. HTTP proxies can't carry UDP, so only
    /// a SOCKS proxy engages this.
    pub fn set_proxy_awareness(&mut self, dns_log: DnsResolutionLog, proxy_env: ProxyEnvironment) {
        self.udp.dns_log = Some(dns_log);
        self.udp.proxy_env = Some(proxy_env);
    }

    /// Dispatches an outbound IPv4 frame to the appropriate protocol proxy.
    ///
    /// Inbound reply frames (matching an active inbound connection) are
    /// intercepted first; everything else is proxied through host sockets.
    /// Non-IPv4 frames are ignored before inbound matching.
    pub fn handle_outbound(&mut self, frame: &[u8], guest_mac: [u8; 6]) {
        if frame.len() < ETH_HEADER_LEN + 20
            || frame[12..14] != [0x08, 0x00]
            || frame[ETH_HEADER_LEN] >> 4 != 4
        {
            return;
        }

        // Check inbound relay first — fast-path ephemeral port range check
        // short-circuits for 99%+ of outbound frames.
        if self.inbound.try_handle_reply(frame, guest_mac) {
            return;
        }

        let protocol = frame[ETH_HEADER_LEN + 9];
        let cancel = self.cancel.clone();
        match protocol {
            1 => self.icmp.proxy_icmp(frame, guest_mac, cancel),
            17 => self.udp.proxy_udp(frame, guest_mac, cancel),
            _ => {
                tracing::trace!("Host egress: dropping protocol {}", protocol);
            }
        }
    }

    /// Handles an inbound UDP command from the listener manager.
    pub fn handle_inbound_command(
        &mut self,
        cmd: super::inbound_relay::InboundCommand,
        guest_mac: [u8; 6],
    ) {
        use super::inbound_relay::InboundCommand;
        if let InboundCommand::UdpReceived {
            host_addr,
            client_addr,
            data,
            reply_tx,
            ..
        } = cmd
        {
            // The guest exposes the host listener's port through Docker DNAT.
            self.inbound.inject_udp(
                super::inbound_relay::InboundUdpPeer {
                    listener: host_addr,
                    client: client_addr,
                },
                &data,
                reply_tx,
                guest_mac,
            );
        }
    }

    /// Runs periodic maintenance (flow cleanup).
    pub fn maintenance(&mut self) {
        self.udp.cleanup_stale_flows();
        self.inbound.cleanup();
    }

    /// Expire a specific flow by address (called from timer wheel).
    ///
    /// This is the timer-wheel-driven counterpart to `maintenance()`.
    /// Instead of scanning all flows, it targets a specific expired flow.
    pub fn expire_flow(&mut self, _addr: std::net::SocketAddr) {
        // TODO: Once UDP/ICMP flows are registered with the timer wheel,
        // this will remove the specific flow by address. For now, the
        // timer wheel is wired into the datapath loop but individual
        // flow registration is not yet migrated from per-flow timeouts.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_egress_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let gw_ip = Ipv4Addr::new(192, 168, 64, 1);
        let gw_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let _egress = HostEgress::new(gw_ip, gw_mac, guest_ip, tx, CancellationToken::new(), 1500);
    }

    #[test]
    fn test_host_egress_drops_unknown_protocol() {
        let (tx, _rx) = mpsc::channel(16);
        let gw_ip = Ipv4Addr::new(192, 168, 64, 1);
        let gw_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let mut egress =
            HostEgress::new(gw_ip, gw_mac, guest_ip, tx, CancellationToken::new(), 1500);

        // Build a minimal Ethernet + IPv4 frame with protocol=50 (ESP).
        let mut frame = vec![0u8; ETH_HEADER_LEN + 20];
        // Ethernet header
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        // IPv4 header
        frame[ETH_HEADER_LEN] = 0x45; // Version 4, IHL 5
        frame[ETH_HEADER_LEN + 9] = 50; // Protocol: ESP (unsupported)

        let guest_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
        // Should not panic.
        egress.handle_outbound(&frame, guest_mac);
    }

    #[test]
    fn test_host_egress_ignores_short_frames() {
        let (tx, _rx) = mpsc::channel(16);
        let gw_ip = Ipv4Addr::new(192, 168, 64, 1);
        let gw_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let mut egress =
            HostEgress::new(gw_ip, gw_mac, guest_ip, tx, CancellationToken::new(), 1500);

        let guest_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
        // Frame shorter than Ethernet + IP minimum.
        egress.handle_outbound(&[0u8; 10], guest_mac);
    }

    fn ipv6_udp_frame(source: std::net::Ipv6Addr, flow_label: u32) -> Vec<u8> {
        use arcbox_packet::checksum::{checksum_add, checksum_fold};
        use arcbox_packet::ip::IpPacketView;

        let destination: std::net::Ipv6Addr = "2001:db8::2".parse().unwrap();
        let mut frame = vec![0u8; ETH_HEADER_LEN + 40 + 12];
        frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        let ip = ETH_HEADER_LEN;
        frame[ip..ip + 4].copy_from_slice(&(0x6000_0000 | flow_label).to_be_bytes());
        frame[ip + 4..ip + 6].copy_from_slice(&12u16.to_be_bytes());
        frame[ip + 6] = 17;
        frame[ip + 7] = 64;
        frame[ip + 8..ip + 24].copy_from_slice(&source.octets());
        frame[ip + 24..ip + 40].copy_from_slice(&destination.octets());
        let udp = ip + 40;
        frame[udp..udp + 2].copy_from_slice(&12345u16.to_be_bytes());
        frame[udp + 2..udp + 4].copy_from_slice(&443u16.to_be_bytes());
        frame[udp + 4..udp + 6].copy_from_slice(&12u16.to_be_bytes());
        frame[udp + 8..].copy_from_slice(b"data");
        let checksum = checksum_fold(
            checksum_add(&source.octets())
                + checksum_add(&destination.octets())
                + 12
                + 17
                + checksum_add(&frame[udp..]),
        );
        let checksum = if checksum == 0 { u16::MAX } else { checksum };
        frame[udp + 6..udp + 8].copy_from_slice(&checksum.to_be_bytes());
        assert_eq!(IpPacketView::parse(&frame[ip..]).unwrap().protocol(), 17);
        frame
    }

    #[test]
    fn ipv6_udp_does_not_dispatch_ipv4_protocols() {
        let (tx, mut rx) = mpsc::channel(16);
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let gateway_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let guest_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
        let mut egress = HostEgress::new(
            gateway_ip,
            gateway_mac,
            guest_ip,
            tx,
            CancellationToken::new(),
            1500,
        );

        // The IPv6 source bytes alias IPv4 UDP/ICMP protocol numbers.
        // Rejection must not require a runtime or start a proxy task.
        for prefix in [0x2011, 0x2001] {
            let source = std::net::Ipv6Addr::new(prefix, 0, 0, 0, 0x7f00, 1, 0, 1);
            let frame = ipv6_udp_frame(source, 443);
            egress.handle_outbound(&frame, guest_mac);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn ipv6_udp_cannot_match_an_ipv4_inbound_flow() {
        use crate::inbound_relay::InboundCommand;
        use arcbox_packet::ethernet::build_udp_ip_ethernet;

        let (tx, mut rx) = mpsc::channel(16);
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let gateway_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let guest_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
        let mut egress = HostEgress::new(
            gateway_ip,
            gateway_mac,
            guest_ip,
            tx,
            CancellationToken::new(),
            1500,
        );
        let (client_tx, mut client_rx) = mpsc::channel(16);
        let client = "127.0.0.1:45678".parse().unwrap();
        // The first two IPv6 bytes alias source port 0x6000 in an IPv4 parse.
        egress.handle_inbound_command(
            InboundCommand::UdpReceived {
                host_addr: "127.0.0.1:24576".parse().unwrap(),
                container_port: 24576,
                data: b"query".to_vec(),
                reply_tx: client_tx,
                client_addr: client,
            },
            guest_mac,
        );
        let injected = rx.try_recv().unwrap();
        let udp = ETH_HEADER_LEN + 20;
        let relay_port = u16::from_be_bytes([injected[udp], injected[udp + 1]]);

        // These source bytes alias the active IPv4 flow under the IPv4 offsets.
        let source = std::net::Ipv6Addr::new(0x2011, 0, 0xc0a8, 0x4002, 0xc0a8, 0x4001, 0, 1);
        let frame = ipv6_udp_frame(source, u32::from(relay_port));
        egress.handle_outbound(&frame, guest_mac);
        assert!(matches!(
            client_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        for frame in build_udp_ip_ethernet(
            guest_ip,
            gateway_ip,
            24576,
            relay_port,
            b"ipv4 reply",
            guest_mac,
            gateway_mac,
            1500,
        ) {
            egress.handle_outbound(&frame, guest_mac);
        }
        assert_eq!(
            client_rx.try_recv().unwrap(),
            (client, b"ipv4 reply".to_vec())
        );
    }

    /// Verifies that reply frames from the egress flow through the mpsc channel.
    #[tokio::test]
    async fn test_reply_channel_flow() {
        let (tx, mut rx) = mpsc::channel(16);
        let gw_ip = Ipv4Addr::new(192, 168, 64, 1);
        let gw_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];

        // Simulate what a proxy task does: send a frame through reply_tx.
        let test_frame = vec![0xDE, 0xAD, 0xBE, 0xEF];
        tx.send(test_frame.clone()).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received, test_frame);

        // Verify the egress creates correctly with the same tx.
        let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
        let _egress = HostEgress::new(gw_ip, gw_mac, guest_ip, tx, CancellationToken::new(), 1500);
    }
}
