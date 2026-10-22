//! Stateless TCP CLOSED replies after upstream resources have been released.

use super::TcpBridge;
use crate::ethernet::{TcpFrameParams, build_tcp_rst_frame, build_tcp_rst_only_frame};
use std::net::IpAddr;

impl TcpBridge {
    /// Returns an RFC 9293 CLOSED-state reply to a complete bare IP packet.
    ///
    /// Returns an Ethernet frame with reversed IP addresses and ports and the supplied MAC addresses.
    /// RST, malformed, fragmented, and non-TCP input produces no reply. No bridge or upstream state is retained.
    /// Unspecified, multicast, and IPv4 limited-broadcast endpoints produce no reply.
    /// Callers must exclude subnet-directed broadcasts using their interface configuration.
    /// Packet structure is validated by the shared parser; offload checksums are not validated.
    #[must_use]
    pub fn closed_reply_ip(
        packet: &[u8],
        gateway_mac: [u8; 6],
        guest_mac: [u8; 6],
    ) -> Option<Vec<u8>> {
        let tcp = super::packet::tcp_at(packet, 0)?;
        let flags = tcp.flags();
        if flags & 0x04 != 0 {
            return None;
        }
        let ip = tcp.ip();
        if [ip.source(), ip.destination()].into_iter().any(|address| {
            address.is_unspecified()
                || address.is_multicast()
                || matches!(address, IpAddr::V4(ip) if ip.is_broadcast())
        }) {
            return None;
        }
        let mut params = TcpFrameParams {
            src_ip: ip.destination(),
            dst_ip: ip.source(),
            src_port: tcp.destination_port(),
            dst_port: tcp.source_port(),
            seq: 0,
            ack: 0,
            window: 0,
            src_mac: gateway_mac,
            dst_mac: guest_mac,
        };
        if flags & 0x10 != 0 {
            params.seq = tcp.acknowledgment();
            Some(build_tcp_rst_only_frame(&params))
        } else {
            params.ack = tcp
                .sequence()
                .wrapping_add(tcp.payload().len() as u32)
                .wrapping_add(u32::from(flags & 0x02 != 0))
                .wrapping_add(u32::from(flags & 0x01 != 0));
            Some(build_tcp_rst_frame(&params))
        }
    }
}
