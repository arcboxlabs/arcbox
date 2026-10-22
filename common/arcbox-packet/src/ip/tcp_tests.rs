use super::*;

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
fn rejects_non_tcp_and_incomplete_segments() {
    let mut packet = scapy_syn(false);
    packet[6] = 17;
    assert!(TcpPacketView::parse(IpPacketView::parse(&packet).unwrap()).is_none());
    packet[6] = 6;
    for length in 0..20 {
        packet[5] = length;
        let ip = IpPacketView::parse(&packet[..40 + usize::from(length)]).unwrap();
        assert!(TcpPacketView::parse(ip).is_none());
    }
}

fn ipv6_frame_params() -> crate::ethernet::TcpFrameParams<std::net::Ipv6Addr> {
    crate::ethernet::TcpFrameParams {
        src_ip: "2001:db8::1".parse().unwrap(),
        dst_ip: "2001:db8::2".parse().unwrap(),
        src_port: 80,
        dst_port: 40000,
        seq: 0x12345678,
        ack: 99,
        window: 32000,
        src_mac: [1; 6],
        dst_mac: [2; 6],
    }
}

#[test]
fn ipv6_control_and_data_frames_have_independent_checksums() {
    use crate::ethernet::*;

    let p = ipv6_frame_params();
    for (frame, flags, len, window) in [
        (build_tcp_ack_frame(&p), 0x10, 20, 32000),
        (build_tcp_data_frame(&p, b"odd"), 0x18, 23, 32000),
        (build_tcp_fin_frame(&p), 0x11, 20, 65535),
        (build_tcp_rst_frame(&p), 0x14, 20, 0),
        (
            build_tcp_data_frame(&p, &vec![0x5a; 65515]),
            0x18,
            65535,
            32000,
        ),
    ] {
        assert_eq!(&frame[..6], &[2; 6]);
        assert_eq!(&frame[6..12], &[1; 6]);
        assert_eq!(&frame[12..14], &[0x86, 0xdd]);
        let ip = &frame[14..];
        assert_eq!(ip.len(), 40 + len);
        assert_eq!((ip[0], ip[6], ip[7]), (0x60, 6, 64));
        assert_eq!(usize::from(u16::from_be_bytes([ip[4], ip[5]])), len);
        assert_eq!(&ip[8..24], &p.src_ip.octets());
        assert_eq!(&ip[24..40], &p.dst_ip.octets());
        assert_eq!(
            &ip[40..53],
            &[0, 80, 0x9c, 0x40, 0x12, 0x34, 0x56, 0x78, 0, 0, 0, 99, 0x50]
        );
        assert_eq!(ip[53], flags);
        assert_eq!(u16::from_be_bytes([ip[54], ip[55]]), window);
        if len == 23 {
            assert_eq!(&ip[60..], b"odd");
        }
        verify_v6(ip, &ip[40..]);
    }
}

#[test]
#[should_panic(expected = "TCP frame exceeds the IP length field")]
fn ipv6_data_frame_rejects_payload_above_length_limit() {
    let _ = crate::ethernet::build_tcp_data_frame(&ipv6_frame_params(), &vec![0; 65516]);
}

#[test]
#[should_panic(expected = "TCP frame addresses must use the same IP family")]
fn tcp_frame_rejects_mixed_address_families() {
    let p = ipv6_frame_params();
    let mixed = crate::ethernet::TcpFrameParams {
        src_ip: IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        dst_ip: IpAddr::from(p.dst_ip),
        src_port: p.src_port,
        dst_port: p.dst_port,
        seq: p.seq,
        ack: p.ack,
        window: p.window,
        src_mac: p.src_mac,
        dst_mac: p.dst_mac,
    };
    let _ = crate::ethernet::build_tcp_ack_frame(&mixed);
}

#[test]
fn syn_frames_preserve_options_fields_and_checksums() {
    use crate::ethernet::*;

    let v6 = ipv6_frame_params();
    for (src_ip, dst_ip) in [
        (IpAddr::from(v6.src_ip), IpAddr::from(v6.dst_ip)),
        ("192.0.2.1".parse().unwrap(), "192.0.2.2".parse().unwrap()),
    ] {
        for (wscale, sack_permitted, options) in [
            (None, false, &[2, 4, 5, 160][..]),
            (None, true, &[2, 4, 5, 160, 4, 2, 1, 1][..]),
            (Some(0), false, &[2, 4, 5, 160, 3, 3, 0, 1][..]),
            (Some(14), true, &[2, 4, 5, 160, 4, 2, 3, 3, 14, 1, 1, 1][..]),
        ] {
            let p = SynAckParams {
                src_ip,
                dst_ip,
                src_port: 80,
                dst_port: 40000,
                seq: 0x12345678,
                ack: 0xfedcba98,
                src_mac: [1; 6],
                dst_mac: [2; 6],
                mss: 1440,
                wscale,
                sack_permitted,
            };
            let active = SynParams {
                src_ip,
                dst_ip,
                src_port: p.src_port,
                dst_port: p.dst_port,
                seq: p.seq,
                src_mac: p.src_mac,
                dst_mac: p.dst_mac,
                mss: p.mss,
                wscale,
                sack_permitted,
            };
            for (frame, flags, ack) in [
                (build_tcp_syn_frame(&active), 0x02, 0u32),
                (build_tcp_syn_ack_frame(&p), 0x12, 0xfedcba98),
            ] {
                assert_eq!(&frame[..12], &[2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1]);
                let ip = &frame[14..];
                let ip_len = if src_ip.is_ipv6() { 40 } else { 20 };
                let tcp = &ip[ip_len..];
                assert_eq!(tcp.len(), 20 + options.len());
                assert_eq!(&tcp[..8], &[0, 80, 0x9c, 0x40, 0x12, 0x34, 0x56, 0x78]);
                assert_eq!(&tcp[8..12], &ack.to_be_bytes());
                assert_eq!(usize::from(tcp[12] >> 4) * 4, tcp.len());
                assert_eq!(&tcp[13..16], &[flags, 255, 255]);
                assert_eq!(&tcp[18..20], &[0, 0]);
                assert_eq!(&tcp[20..], options);
                if src_ip.is_ipv6() {
                    assert_eq!(&frame[12..14], &[0x86, 0xdd]);
                    assert_eq!(usize::from(u16::from_be_bytes([ip[4], ip[5]])), tcp.len());
                    verify_v6(ip, tcp);
                } else {
                    assert_eq!(&frame[12..14], &[8, 0]);
                    assert_eq!(usize::from(u16::from_be_bytes([ip[2], ip[3]])), ip.len());
                    assert_eq!(folded_sum(&[&ip[..20]]), 0xffff);
                    let length = (tcp.len() as u16).to_be_bytes();
                    assert_eq!(folded_sum(&[&ip[12..20], &[0, 6], &length, tcp]), 0xffff);
                }
            }
        }
    }
}
