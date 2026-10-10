use super::*;
use arcbox_packet::ethernet::{TcpFrameParams, build_tcp_data_frame, build_udp_ip_ethernet};

use crate::shim::{GATEWAY_MAC, HOST_MAC};

const HOST: Ipv4Addr = Ipv4Addr::new(192, 168, 64, 2);
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 64, 1);

fn udp(payload: &[u8], port: u16, mtu: usize) -> Vec<Vec<u8>> {
    build_udp_ip_ethernet(
        HOST,
        GATEWAY,
        40000,
        port,
        payload,
        HOST_MAC,
        GATEWAY_MAC,
        mtu,
    )
}

#[test]
fn l3_classification_retains_mixed_protocol_bytes_and_order() {
    let mut device = FrameClassifier::with_pool_capacity(GATEWAY, 4000, 1);
    let mut mac = None;
    let params = TcpFrameParams {
        src_ip: HOST,
        dst_ip: GATEWAY,
        src_port: 40000,
        dst_port: 443,
        seq: 1234,
        ack: 5678,
        window: 4096,
        src_mac: HOST_MAC,
        dst_mac: GATEWAY_MAC,
    };
    let tcp = build_tcp_data_frame(&params, b"first");
    let mut syn = build_tcp_data_frame(&params, &[]);
    syn[ETH_HEADER_LEN + 20 + 13] = 0x02;
    let mut frames = vec![tcp.clone(), syn.clone()];
    for port in [53, 67, 9999] {
        frames.extend(udp(b"datagram", port, 4000));
    }
    let mut icmp = udp(b"icmp", 0, 4000).remove(0);
    icmp[ETH_HEADER_LEN + 9] = PROTO_ICMP;
    frames.push(icmp);
    frames.push(build_tcp_data_frame(&params, b"last"));
    for frame in &frames {
        device.classify_ip_packet(&frame[ETH_HEADER_LEN..], &mut mac);
    }
    assert_eq!(mac, Some(HOST_MAC));
    assert_eq!(device.rx_queue.len(), 2);
    assert!(device.rx_queue[0].is_pooled());
    assert!(!device.rx_queue[1].is_pooled());
    assert_eq!(&*device.rx_queue[0], tcp);
    assert_eq!(&*device.rx_queue[1], frames.last().unwrap());
    assert_eq!(device.gated_syns.len(), 1);
    let gated = &device.gated_syns[0];
    assert_eq!(
        (
            gated.src_ip,
            gated.dst_ip,
            gated.src_port,
            gated.dst_port,
            gated.syn_seq
        ),
        (HOST, GATEWAY, 40000, 443, 1234)
    );
    assert_eq!(gated.frame, syn);
    let kinds = [
        InterceptedKind::Dns,
        InterceptedKind::Dhcp,
        InterceptedKind::Udp,
        InterceptedKind::Icmp,
    ];
    assert_eq!(device.intercepted.len(), kinds.len());
    for ((retained, expected), kind) in device.intercepted.iter().zip(&frames[2..6]).zip(kinds) {
        assert_eq!(retained.kind, kind);
        assert_eq!(&*retained.frame, expected);
    }
    for frame in &mut frames {
        frame.fill(0);
    }
    assert_eq!(&*device.rx_queue[0], tcp);
}

#[test]
fn l3_rejects_invalid_headers_and_oversized_input() {
    let mut device = FrameClassifier::with_pool_capacity(GATEWAY, 4000, 1);
    let mut mac = None;
    let valid = udp(b"data", 9999, 4000).remove(0)[ETH_HEADER_LEN..].to_vec();
    for len in 0..28 {
        device.classify_ip_packet(&valid[..len], &mut mac);
    }
    for version_ihl in [0x44, 0x65, 0x4F] {
        let mut invalid = valid.clone();
        invalid[0] = version_ihl;
        device.classify_ip_packet(&invalid, &mut mac);
    }
    let mut oversized = vec![0; 65536];
    oversized[..valid.len()].copy_from_slice(&valid);
    device.classify_ip_packet(&oversized, &mut mac);
    assert!(device.rx_queue.is_empty());
    assert!(device.gated_syns.is_empty());
    assert!(device.intercepted.is_empty());
    assert_eq!(device.pool.free_count(), 1);
}

#[test]
fn l3_reassembles_out_of_order_fragments_before_interception() {
    let payload: Vec<u8> = (0..4000).map(|index| (index % 251) as u8).collect();
    let mut device = FrameClassifier::with_pool_capacity(GATEWAY, 4000, 1);
    let mut mac = None;
    let mut fragments = udp(&payload, 53, 1500);
    assert_eq!(fragments.len(), 3);
    for fragment in fragments.drain(1..).rev() {
        device.classify_ip_packet(&fragment[ETH_HEADER_LEN..], &mut mac);
        assert!(device.intercepted.is_empty());
    }
    device.classify_ip_packet(&fragments[0][ETH_HEADER_LEN..], &mut mac);
    assert_eq!(device.intercepted.len(), 1);
    let frame = &device.intercepted[0];
    assert_eq!(frame.kind, InterceptedKind::Dns);
    assert_eq!(
        &frame.frame[..ETH_HEADER_LEN],
        crate::shim::synthetic_l2_header()
    );
    let packet = &frame.frame[ETH_HEADER_LEN..];
    assert_eq!(u16::from_be_bytes([packet[2], packet[3]]) as usize, 4028);
    assert_eq!(&packet[6..8], &[0, 0]);
    assert_eq!(&packet[28..], payload);
    assert_eq!(
        arcbox_packet::ethernet::ipv4_header_checksum(&packet[..20]),
        u16::from_be_bytes([packet[10], packet[11]])
    );
}
