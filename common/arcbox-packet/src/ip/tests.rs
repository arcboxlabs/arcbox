use super::*;
use crate::ethernet::*;
use std::net::Ipv6Addr;

// Wire facts: Scapy v2.6.1, ::1 TCP sport=20/dport=80 SYN, window=8192.
// https://github.com/secdev/scapy/blob/v2.6.1/test/scapy/layers/inet6.uts#L18
// The fixture is literal packet data; neither parser nor encoder creates the expected checksum.
fn scapy_syn(payload: bool) -> Vec<u8> {
    let mut packet = vec![0; 40];
    packet[0] = 0x60;
    packet[5] = if payload { 28 } else { 20 };
    packet[6] = 6;
    packet[7] = 64;
    packet[23] = 1;
    packet[39] = 1;
    packet.extend_from_slice(&[
        0, 20, 0, 80, 0, 0, 0, 0, 0, 0, 0, 0, 0x50, 2, 0x20, 0, 0x8f, 0x7d, 0, 0,
    ]);
    if payload {
        packet[56..58].copy_from_slice(&[0xd5, 0xdd]);
        packet.extend_from_slice(b"somedata");
    }
    packet
}

fn folded_sum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u64;
    let mut high = None;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        if let Some(first) = high.take() {
            sum += u64::from(u16::from_be_bytes([first, *byte]));
        } else {
            high = Some(*byte);
        }
    }
    if let Some(first) = high {
        sum += u64::from(first) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

fn verify_v6(packet: &[u8], tcp: &[u8]) {
    let length = (tcp.len() as u32).to_be_bytes();
    assert_eq!(
        folded_sum(&[&packet[8..40], &length, &[0, 0, 0, 6], tcp]),
        0xffff
    );
}

#[test]
fn parses_independent_scapy_ipv6_tcp_vectors_and_extension_chain() {
    for payload in [false, true] {
        let packet = scapy_syn(payload);
        let tcp = TcpPacketView::parse(IpPacketView::parse(&packet).unwrap()).unwrap();
        assert_eq!(tcp.source_port(), 20);
        assert_eq!(tcp.destination_port(), 80);
        assert_eq!(tcp.flags(), 2);
        assert_eq!(tcp.window(), 8192);
        assert_eq!(tcp.payload(), if payload { &b"somedata"[..] } else { &[] });
        verify_v6(&packet, tcp.segment());
        let mut extended = packet[..40].to_vec();
        extended[6] = 0;
        extended[5] += 16;
        extended.extend_from_slice(&[60, 0, 0, 0, 0, 0, 0, 0]);
        extended.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0]);
        extended.extend_from_slice(&packet[40..]);
        extended.extend_from_slice(&[0xee; 12]);
        let tcp = TcpPacketView::parse(IpPacketView::parse(&extended).unwrap()).unwrap();
        assert_eq!(tcp.segment(), &packet[40..]);
        assert_eq!(tcp.ip().packet().len(), packet.len() + 16);
        verify_v6(&extended, tcp.segment());
    }
}

#[test]
fn parses_independent_smoltcp_ipv4_syn_payload_vector() {
    // smoltcp 0.13.1 SYN_PACKET_BYTES (0BSD), 192.168.1.1 -> 192.168.1.2.
    // https://github.com/smoltcp-rs/smoltcp/blob/e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897/src/wire/tcp.rs#L1287
    let tcp = [
        0xbf, 0, 0, 0x50, 1, 0x23, 0x45, 0x67, 0, 0, 0, 0, 0x50, 2, 1, 0x23, 0x7a, 0x8d, 0, 0,
        0xaa, 0, 0, 0xff,
    ];
    let mut packet = vec![0; 20];
    packet[0] = 0x45;
    packet[3] = 44;
    packet[9] = 6;
    packet[12..20].copy_from_slice(&[192, 168, 1, 1, 192, 168, 1, 2]);
    packet.extend_from_slice(&tcp);
    let view = TcpPacketView::parse(IpPacketView::parse(&packet).unwrap()).unwrap();
    assert_eq!(view.sequence(), 0x01234567);
    assert_eq!(view.source_port(), 48896);
    assert_eq!(view.payload(), &[0xaa, 0, 0, 0xff]);
    assert_eq!(folded_sum(&[&packet[12..20], &[0, 6, 0, 24], &tcp]), 0xffff);
}

#[test]
fn rejects_truncated_extensions_fragments_and_tcp_headers() {
    let packet = scapy_syn(false);
    for length in 0..packet.len() {
        assert!(IpPacketView::parse(&packet[..length]).is_none());
    }
    let mut malformed = packet.clone();
    malformed[6] = 60;
    malformed[41] = 255;
    assert!(IpPacketView::parse(&malformed).is_none());
    malformed = packet.clone();
    malformed[52] = 0x40;
    assert!(TcpPacketView::parse(IpPacketView::parse(&malformed).unwrap()).is_none());
    malformed[52] = 0xf0;
    assert!(TcpPacketView::parse(IpPacketView::parse(&malformed).unwrap()).is_none());
    let mut fragmented = packet[..40].to_vec();
    fragmented[6] = 44;
    fragmented[5] += 8;
    fragmented.extend_from_slice(&[6, 0, 0, 1, 0, 0, 0, 7]);
    fragmented.extend_from_slice(&packet[40..]);
    let ip = IpPacketView::parse(&fragmented).unwrap();
    assert!(ip.transport().is_none());
    assert!(TcpPacketView::parse(ip).is_none());
    assert_eq!(ip.fragment().unwrap().id, 7);
    fragmented[43] = 0;
    assert!(
        IpPacketView::parse(&fragmented)
            .unwrap()
            .transport()
            .is_none(),
        "atomic fragments must pass through reassembly too"
    );
}

#[test]
fn ipv6_builders_have_independent_checksums_and_lengths() {
    let params = TcpFrameParams {
        src_ip: "2001:db8::1".parse::<Ipv6Addr>().unwrap(),
        dst_ip: "2001:db8::2".parse::<Ipv6Addr>().unwrap(),
        src_port: 80,
        dst_port: 40000,
        seq: 0x12345678,
        ack: 99,
        window: 32000,
        src_mac: [1; 6],
        dst_mac: [2; 6],
    };
    let syn = SynParams {
        src_ip: params.src_ip,
        dst_ip: params.dst_ip,
        src_port: params.src_port,
        dst_port: params.dst_port,
        seq: params.seq,
        src_mac: params.src_mac,
        dst_mac: params.dst_mac,
        mss: 1440,
        wscale: Some(7),
        sack_permitted: true,
    };
    let syn_ack = SynAckParams {
        src_ip: syn.src_ip,
        dst_ip: syn.dst_ip,
        src_port: syn.src_port,
        dst_port: syn.dst_port,
        seq: syn.seq,
        ack: 99,
        src_mac: syn.src_mac,
        dst_mac: syn.dst_mac,
        mss: 1440,
        wscale: Some(7),
        sack_permitted: true,
    };
    let frames = [
        (build_tcp_ack_frame(&params), 0x10, 20),
        (build_tcp_data_frame(&params, b"odd"), 0x18, 23),
        (build_tcp_fin_frame(&params), 0x11, 20),
        (build_tcp_rst_frame(&params), 0x14, 20),
        (build_tcp_syn_frame(&syn), 0x02, 32),
        (build_tcp_syn_ack_frame(&syn_ack), 0x12, 32),
        (
            build_tcp_data_frame(&params, &vec![0x5a; 65515]),
            0x18,
            65535,
        ),
    ];
    for (frame, flags, len) in frames {
        assert_eq!(&frame[12..14], &[0x86, 0xdd]);
        let ip = &frame[14..];
        assert_eq!(ip.len(), 40 + len);
        assert_eq!(usize::from(u16::from_be_bytes([ip[4], ip[5]])), len);
        assert_eq!(ip[53], flags);
        verify_v6(ip, &ip[40..]);
    }
}

#[test]
fn validates_extension_option_lengths_and_discard_actions() {
    let tcp = scapy_syn(false);
    for (extension, accepted) in [
        ([6, 0, 0x22, 2, 1, 2, 0, 0], true),
        ([6, 0, 1, 7, 0, 0, 0, 0], false),
        ([6, 0, 0, 0, 0, 0, 0, 2], false),
        ([6, 0, 0x40, 2, 0, 0, 0, 0], false),
        ([6, 0, 0x80, 2, 0, 0, 0, 0], false),
        ([6, 0, 0xc2, 4, 0, 1, 0, 0], false),
    ] {
        for next in [0, 60] {
            let mut packet = tcp[..40].to_vec();
            packet[6] = next;
            packet[5] += 8;
            packet.extend_from_slice(&extension);
            packet.extend_from_slice(&tcp[40..]);
            assert_eq!(IpPacketView::parse(&packet).is_some(), accepted);
        }
    }
}
