//! TCP CLOSED reply flags, sequence space, checksums, and parser boundaries.

use super::*;

mod abort;
use crate::ethernet::{TcpFrameParams, build_tcp_data_frame};
use arcbox_packet::{
    checksum,
    ip::{IpPacketView, TcpPacketView},
};

fn key() -> SynFlowKey {
    SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40400,
        dst_ip: Ipv4Addr::new(198, 18, 0, 1).into(),
        dst_port: 443,
    }
}

fn checksum_sum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|word| u32::from(u16::from_be_bytes([word[0], *word.get(1).unwrap_or(&0)])))
        .sum();
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    sum as u16
}

fn assert_reset(frame: &[u8], key: SynFlowKey, expected: (u32, u32, u8)) {
    assert_eq!(&frame[..6], &GUEST_MAC);
    assert_eq!(&frame[6..12], &GW_MAC);
    let ip = &frame[ETH_HEADER_LEN..];
    let (tcp, mut pseudo) = match (key.dst_ip, key.src_ip) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => {
            assert_eq!(&frame[12..14], &[0x08, 0x00]);
            assert_eq!(ip.len(), 40);
            assert_eq!(checksum_sum(&ip[..20]), 65535);
            let mut pseudo = src.octets().to_vec();
            pseudo.extend_from_slice(&dst.octets());
            pseudo.extend_from_slice(&[0, 6, 0, 20]);
            (&ip[20..], pseudo)
        }
        (IpAddr::V6(src), IpAddr::V6(dst)) => {
            assert_eq!(&frame[12..14], &[0x86, 0xdd]);
            assert_eq!(ip.len(), 60);
            let mut pseudo = src.octets().to_vec();
            pseudo.extend_from_slice(&dst.octets());
            pseudo.extend_from_slice(&[0, 0, 0, 20, 0, 0, 0, 6]);
            (&ip[40..], pseudo)
        }
        _ => panic!("one address family per flow"),
    };
    pseudo.extend_from_slice(tcp);
    assert_eq!(checksum_sum(&pseudo), 65535, "TCP checksum");
    let packet = TcpPacketView::parse(IpPacketView::parse(ip).unwrap()).unwrap();
    assert_eq!(packet.ip().source(), key.dst_ip);
    assert_eq!(packet.ip().destination(), key.src_ip);
    assert_eq!(packet.source_port(), key.dst_port);
    assert_eq!(packet.destination_port(), key.src_port);
    assert_eq!(packet.sequence(), expected.0);
    assert_eq!(packet.acknowledgment(), expected.1);
    assert_eq!(packet.flags(), expected.2);
    assert_eq!(packet.window(), 0);
    assert!(packet.payload().is_empty());
}

fn guest_packet(key: SynFlowKey, cursors: (u32, u32), flags: u8, payload: &[u8]) -> Vec<u8> {
    let frame = build_tcp_data_frame(
        &TcpFrameParams {
            src_ip: key.src_ip,
            dst_ip: key.dst_ip,
            src_port: key.src_port,
            dst_port: key.dst_port,
            seq: cursors.0,
            ack: cursors.1,
            window: 65535,
            src_mac: GUEST_MAC,
            dst_mac: GW_MAC,
        },
        payload,
    );
    let mut packet = frame[ETH_HEADER_LEN..].to_vec();
    let tcp = if key.src_ip.is_ipv6() { 40 } else { 20 };
    let old_flags = u16::from_be_bytes([packet[tcp + 12], packet[tcp + 13]]);
    let new_flags = u16::from_be_bytes([packet[tcp + 12], flags]);
    let old_checksum = u16::from_be_bytes([packet[tcp + 16], packet[tcp + 17]]);
    let new_checksum = checksum::incremental_checksum_update(old_checksum, old_flags, new_flags);
    packet[tcp + 13] = flags;
    packet[tcp + 16..tcp + 18].copy_from_slice(&new_checksum.to_be_bytes());
    packet
}

#[test]
fn closed_replies_follow_ack_precedence_and_count_syn_fin_payload_with_wraparound() {
    for ipv6 in [false, true] {
        let mut key = key();
        if ipv6 {
            key.src_ip = "2001:db8::2".parse().unwrap();
            key.dst_ip = "2001:db8::1".parse().unwrap();
        }
        for (flags, expected) in [
            (0x10, (731, 0, 0x04)),
            (0x18, (731, 0, 0x04)),
            (0x13, (731, 0, 0x04)),
            (0x00, (0, 1, 0x14)),
            (0x02, (0, 2, 0x14)),
            (0x01, (0, 2, 0x14)),
            (0x03, (0, 3, 0x14)),
        ] {
            let packet = guest_packet(key, (u32::MAX - 1, 731), flags, b"abc");
            let reply = TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).unwrap();
            assert_reset(&reply, key, expected);
        }
        for flags in [0x04, 0x14, 0x1f] {
            let packet = guest_packet(key, (10, 20), flags, b"must not trigger a reset loop");
            assert!(TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).is_none());
        }
    }
}

#[test]
fn closed_reply_rejects_non_unicast_endpoints_without_restricting_local_unicast() {
    for (address, reply_expected) in [
        ("0.0.0.0", false),
        ("224.0.0.1", false),
        ("239.255.255.255", false),
        ("255.255.255.255", false),
        ("::", false),
        ("ff02::1", false),
        ("ff05::2", false),
        ("127.0.0.1", true),
        ("169.254.1.2", true),
        ("::1", true),
        ("fe80::2", true),
        ("fd00::2", true),
    ] {
        let address: IpAddr = address.parse().unwrap();
        for source in [false, true] {
            let mut key = key();
            if address.is_ipv6() {
                key.src_ip = "2001:db8::2".parse().unwrap();
                key.dst_ip = "2001:db8::1".parse().unwrap();
            }
            if source {
                key.src_ip = address;
            } else {
                key.dst_ip = address;
            }
            for flags in [0x02, 0x10] {
                let packet = guest_packet(key, (1000, 2000), flags, &[]);
                let reply = TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC);
                assert_eq!(
                    reply.is_some(),
                    reply_expected,
                    "{address}, source={source}, flags={flags}"
                );
            }
        }
    }
}

#[test]
fn closed_reply_ignores_tcp_options_and_bytes_after_the_ip_end() {
    let key = key();
    let mut packet = guest_packet(key, (900, 0), 0x02, b"abc");
    packet.splice(40..40, [1, 1, 1, 1]);
    packet[32] = 0x60;
    packet[2..4].copy_from_slice(&47u16.to_be_bytes());
    packet.extend_from_slice(b"padding outside the declared packet");
    let reply = TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).unwrap();
    assert_reset(&reply, key, (0, 904, 0x14));
}

#[test]
fn closed_reply_parses_ipv6_extensions_but_rejects_fragments_and_malformed_packets() {
    for ipv6 in [false, true] {
        let mut key = key();
        if ipv6 {
            key.src_ip = "2001:db8::2".parse().unwrap();
            key.dst_ip = "2001:db8::1".parse().unwrap();
        }
        let mut packet = guest_packet(key, (1000, 2000), 0x10, &[]);
        let tcp = if ipv6 { 40 } else { 20 };
        for end in [0, tcp - 1, packet.len() - 1] {
            assert!(TcpBridge::closed_reply_ip(&packet[..end], GW_MAC, GUEST_MAC).is_none());
        }
        packet[tcp + 12] = 0xf0;
        assert!(TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).is_none());
        packet[tcp + 12] = 0x50;
        if ipv6 {
            packet[6] = 60;
            packet[4..6].copy_from_slice(&28u16.to_be_bytes());
            packet.splice(40..40, [6, 0, 0, 0, 0, 0, 0, 0]);
            let reply = TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).unwrap();
            assert_reset(&reply, key, (2000, 0, 0x04));
            packet[6] = 44;
            packet[43] = 1;
        } else {
            packet[6..8].copy_from_slice(&0x2000u16.to_be_bytes());
        }
        assert!(TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).is_none());
        let mut packet = guest_packet(key, (1000, 2000), 0x10, &[]);
        packet[if ipv6 { 6 } else { 9 }] = 17;
        assert!(TcpBridge::closed_reply_ip(&packet, GW_MAC, GUEST_MAC).is_none());
    }
}
