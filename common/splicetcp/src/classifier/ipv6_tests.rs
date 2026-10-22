use super::*;
use crate::{
    shim::{GATEWAY_MAC, HOST_MAC},
    tcp_bridge::TcpBridge,
};
use arcbox_packet::ethernet::{
    SynParams, TcpFrameParams, build_tcp_data_frame, build_tcp_syn_frame,
};
use std::{net::Ipv6Addr, time::Duration};
use tokio::{io::AsyncReadExt, sync::Notify};

fn feed(classifier: &mut FrameClassifier, frame: &[u8], bare: bool) {
    if bare {
        classifier.classify_ip_packet(&frame[ETH_HEADER_LEN..], &mut None);
    } else {
        classifier.classify_frame(frame, &mut None);
    }
}

fn fragments(frame: &[u8]) -> Vec<Vec<u8>> {
    let tcp = &frame[54..];
    tcp.chunks(32)
        .enumerate()
        .map(|(index, part)| {
            let mut fragment = frame[..54].to_vec();
            fragment[20] = 44;
            fragment[18..20].copy_from_slice(&(8 + part.len() as u16).to_be_bytes());
            let offset = (index * 32) as u16 | u16::from((index + 1) * 32 < tcp.len());
            fragment.extend_from_slice(&[6, 0]);
            fragment.extend_from_slice(&offset.to_be_bytes());
            fragment.extend_from_slice(&0x12345678u32.to_be_bytes());
            fragment.extend_from_slice(part);
            fragment
        })
        .collect()
}

#[tokio::test]
async fn ipv6_classification_reassembles_handshake_payload_into_owned_socket() {
    for bare in [false, true] {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let guest: IpAddr = "2001:db8::2".parse().unwrap();
            let remote: IpAddr = Ipv6Addr::LOCALHOST.into();
            let port = listener.local_addr().unwrap().port();
            let mut classifier = FrameClassifier::with_pool_capacity(Ipv4Addr::LOCALHOST, 1500, 1);
            let mut bridge = TcpBridge::new(Ipv4Addr::new(192, 168, 64, 1));
            bridge.set_fast_path_macs(GATEWAY_MAC, HOST_MAC);
            let wake = Arc::new(Notify::new());
            bridge.set_handshake_waker(Arc::clone(&wake));
            let syn = build_tcp_syn_frame(&SynParams {
                src_ip: guest,
                dst_ip: remote,
                src_port: 44000,
                dst_port: port,
                seq: u32::MAX,
                src_mac: HOST_MAC,
                dst_mac: GATEWAY_MAC,
                mss: 1220,
                wscale: None,
                sack_permitted: false,
            });
            feed(&mut classifier, &syn, bare);
            let gated = classifier.take_gated_syns();
            assert_eq!(gated.len(), 1);
            assert_eq!((gated[0].src_ip, gated[0].dst_ip), (guest, remote));
            assert!(
                bridge
                    .handle_outbound_syn(&gated[0].frame, GATEWAY_MAC, HOST_MAC)
                    .is_none()
            );
            let (mut server, _) = listener.accept().await.unwrap();
            wake.notified().await;
            let syn_ack = bridge.poll_handshakes();
            assert_eq!(syn_ack.len(), 1);
            let reply =
                TcpPacketView::parse(IpPacketView::parse(&syn_ack[0][14..]).unwrap()).unwrap();
            assert_eq!((reply.flags(), reply.acknowledgment()), (0x12, 0));
            let payload = b"fragmented final ACK reaches the IPv6 socket once";
            let data = build_tcp_data_frame(
                &TcpFrameParams {
                    src_ip: guest,
                    dst_ip: remote,
                    src_port: 44000,
                    dst_port: port,
                    seq: 0,
                    ack: reply.sequence().wrapping_add(1),
                    window: 1234,
                    src_mac: HOST_MAC,
                    dst_mac: GATEWAY_MAC,
                },
                payload,
            );
            let fragments = fragments(&data);
            assert!(fragments.len() > 1);
            for fragment in fragments.iter().skip(1).rev() {
                feed(&mut classifier, fragment, bare);
                assert!(classifier.drain_tcp(&mut bridge).is_empty());
                assert_eq!(bridge.handshake_count(), 1);
                assert_eq!(bridge.fast_path_count(), 0);
            }
            feed(&mut classifier, &fragments[0], bare);
            let replies = classifier.drain_tcp(&mut bridge);
            assert_eq!(replies.len(), 1);
            let ack =
                TcpPacketView::parse(IpPacketView::parse(&replies[0][14..]).unwrap()).unwrap();
            assert_eq!(ack.acknowledgment(), payload.len() as u32);
            assert_eq!(bridge.handshake_count(), 0);
            assert_eq!(bridge.fast_path_count(), 1);
            feed(&mut classifier, &data, bare);
            assert_eq!(classifier.drain_tcp(&mut bridge).len(), 1);
            drop(bridge);
            let mut received = Vec::new();
            server.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, payload);
        })
        .await
        .unwrap();
    }
}

#[test]
fn ipv6_udp_classification_preserves_maximum_packet_and_protocol_family() {
    for bare in [false, true] {
        let mut classifier = FrameClassifier::with_pool_capacity(Ipv4Addr::LOCALHOST, 4000, 1);
        for length in [16u16, u16::MAX] {
            for port in [53u16, 67, 443] {
                let mut packet = vec![0x5a; 40 + usize::from(length)];
                packet[..40].fill(0);
                packet[0] = 0x60;
                packet[4..6].copy_from_slice(&length.to_be_bytes());
                packet[6] = 17;
                packet[7] = 64;
                packet[23] = 1;
                packet[39] = 2;
                packet[42..44].copy_from_slice(&port.to_be_bytes());
                packet[44..46].copy_from_slice(&length.to_be_bytes());
                packet[46..48].fill(0);
                let sum = arcbox_packet::checksum::checksum_add(&packet[8..40])
                    + u32::from(length)
                    + 17
                    + arcbox_packet::checksum::checksum_add(&packet[40..]);
                let checksum = arcbox_packet::checksum::checksum_fold(sum);
                packet[46..48].copy_from_slice(
                    &if checksum == 0 { u16::MAX } else { checksum }.to_be_bytes(),
                );
                let mut frame = crate::shim::synthetic_l2_header().to_vec();
                frame[12..14].copy_from_slice(&[0x86, 0xdd]);
                frame.extend_from_slice(&packet);
                feed(&mut classifier, &frame, bare);
                let retained = classifier.take_intercepted();
                assert_eq!(retained.len(), 1);
                assert_eq!(retained[0].kind, InterceptedKind::Udp);
                assert_eq!(&retained[0].frame[ETH_HEADER_LEN..], packet);
                if length == u16::MAX {
                    assert!(!retained[0].frame.is_pooled());
                    packet.push(0);
                    classifier.classify_ip_packet(&packet, &mut None);
                    assert!(classifier.take_intercepted().is_empty());
                }
            }
        }
    }
}
