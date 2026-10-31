//! Stateless TCP CLOSED replies after upstream resources have been released.

use super::{SynFlowKey, TcpBridge};
use crate::ethernet::{
    ETH_HEADER_LEN, TcpFrameParams, build_tcp_rst_frame, build_tcp_rst_only_frame,
};
use std::net::IpAddr;

impl TcpBridge {
    /// Returns a CLOSED-state reply to a guest Ethernet frame whose flow the bridge does not track.
    ///
    /// Covers segments of connections the bridge never saw, such as those a restored
    /// guest still holds, and of flows whose state was already released.
    /// Bare SYNs, tracked flows, and frames before the guest MAC is learned produce no reply.
    #[must_use]
    pub fn untracked_reply(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let guest_mac = self.fast_path_guest_mac?;
        let tcp = super::packet::tcp_at(frame, ETH_HEADER_LEN)?;
        if tcp.flags() & 0x12 == 0x02 {
            return None;
        }
        let key = SynFlowKey {
            src_ip: tcp.ip().source(),
            src_port: tcp.source_port(),
            dst_ip: tcp.ip().destination(),
            dst_port: tcp.destination_port(),
        };
        if self.fast_path_conns.contains_key(&key)
            || self.handshake_conns.contains_key(&key)
            || self.abandoned_conns.contains_key(&key)
            || self.flow_controls.contains_key(&key)
        {
            return None;
        }
        Self::closed_reply_ip(
            &frame[ETH_HEADER_LEN..],
            self.fast_path_gateway_mac,
            guest_mac,
        )
    }

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
