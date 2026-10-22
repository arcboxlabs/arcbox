//! Validate the IP/TCP boundary for Ethernet and borrowed IP input.

use crate::ethernet::ETH_HEADER_LEN;
use arcbox_packet::ip::{IpPacketView, TcpPacketView};

pub(super) fn tcp_at(frame: &[u8], ip_start: usize) -> Option<TcpPacketView<'_>> {
    let ip = IpPacketView::parse(frame.get(ip_start..)?)?;
    // Only Ethernet input may exceed the address family's maximum packet length with padding.
    if ip_start == 0 {
        let max_len = if ip.source().is_ipv6() {
            40 + usize::from(u16::MAX)
        } else {
            usize::from(u16::MAX)
        };
        if frame.len() > max_len {
            return None;
        }
    }
    if ip_start == ETH_HEADER_LEN {
        let expected = if ip.source().is_ipv6() {
            [0x86, 0xdd]
        } else {
            [0x08, 0x00]
        };
        if frame[12..14] != expected {
            return None;
        }
    }
    TcpPacketView::parse(ip)
}
