use super::*;
use arcbox_packet::{checksum, ip::TcpPacketView};
use std::net::{Ipv6Addr, Shutdown};
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(2);

fn key(port: u16) -> SynFlowKey {
    SynFlowKey {
        src_ip: "2001:db8::2".parse().unwrap(),
        src_port: 41101,
        dst_ip: Ipv6Addr::LOCALHOST.into(),
        dst_port: port,
    }
}

fn guest_frame(key: SynFlowKey, ack: u32, options: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 82 + options.len() + payload.len()];
    frame[..6].copy_from_slice(&GW_MAC);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
    frame[14] = 0x60;
    let length = u16::try_from(frame.len() - 54).unwrap();
    frame[18..20].copy_from_slice(&length.to_be_bytes());
    frame[21] = 64;
    let (IpAddr::V6(src), IpAddr::V6(dst)) = (key.src_ip, key.dst_ip) else {
        panic!("IPv6 fixture")
    };
    frame[22..38].copy_from_slice(&src.octets());
    frame[38..54].copy_from_slice(&dst.octets());
    frame[54] = 6; // One Hop-by-Hop header precedes TCP.
    let tcp = &mut frame[62..];
    tcp[..2].copy_from_slice(&key.src_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&key.dst_port.to_be_bytes());
    tcp[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    tcp[8..12].copy_from_slice(&ack.to_be_bytes());
    tcp[12] = (((20 + options.len()) / 4) as u8) << 4;
    tcp[13] = 0x02;
    tcp[14..16].copy_from_slice(&1234u16.to_be_bytes());
    tcp[20..20 + options.len()].copy_from_slice(options);
    tcp[20 + options.len()..].copy_from_slice(payload);
    seal(&mut frame);
    frame
}

fn seal(frame: &mut [u8]) {
    frame[78..80].fill(0);
    let sum = checksum::checksum_add(&frame[22..54])
        + (frame.len() - 62) as u32
        + 6
        + checksum::checksum_add(&frame[62..]);
    frame[78..80].copy_from_slice(&checksum::checksum_fold(sum).to_be_bytes());
}

fn checked_reply(frame: &[u8], key: SynFlowKey) -> TcpPacketView<'_> {
    let tcp = super::super::packet::tcp_at(frame, ETH_HEADER_LEN).unwrap();
    assert_eq!(tcp.ip().source(), key.dst_ip);
    assert_eq!(tcp.ip().destination(), key.src_ip);
    assert_eq!(tcp.source_port(), key.dst_port);
    assert_eq!(tcp.destination_port(), key.src_port);
    let sum = checksum::checksum_add(&frame[22..54])
        + tcp.segment().len() as u32
        + 6
        + checksum::checksum_add(tcp.segment());
    assert_eq!(checksum::checksum_fold(sum), 0);
    tcp
}

fn rejects_malformed(bridge: &mut TcpBridge, frame: &[u8]) {
    let pending = bridge.handshake_count();
    let established = bridge.fast_path_count();
    let mut inputs: Vec<_> = (0..frame.len()).map(|len| frame[..len].to_vec()).collect();
    for (offset, value) in [
        (12, 0x08),
        (14, 0x40),
        (18, 0xff),
        (20, 17),
        (54, 44),
        (74, 0x40),
    ] {
        let mut malformed = frame.to_vec();
        malformed[offset] = value;
        inputs.push(malformed);
    }
    for input in inputs {
        assert!(
            bridge
                .handle_outbound_syn(&input, GW_MAC, GUEST_MAC)
                .is_none()
        );
        assert!(bridge.try_complete_handshake(&input).is_none());
        assert_eq!(bridge.handshake_count(), pending);
        assert_eq!(bridge.fast_path_count(), established);
    }
}

#[tokio::test]
async fn passive_ipv6_handshake_delivers_final_ack_payload() {
    for options in [&[][..], &[2, 4, 3, 132, 1, 3, 3, 2][..]] {
        tokio::time::timeout(TIMEOUT, async {
            let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let key = key(listener.local_addr().unwrap().port());
            let mut bridge = TcpBridge::new(GW_IP);
            bridge.set_fast_path_mtu(4000);
            bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
            let wake = Arc::new(Notify::new());
            bridge.set_handshake_waker(Arc::clone(&wake));
            let syn = guest_frame(key, 0, options, &[]);
            rejects_malformed(&mut bridge, &syn);
            assert!(
                bridge
                    .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
                    .is_none()
            );
            let (mut server, _) = listener.accept().await.unwrap();
            wake.notified().await;
            let first = bridge.poll_handshakes();
            assert_eq!(first.len(), 1);
            let reply = checked_reply(&first[0], key);
            assert_eq!((reply.flags(), reply.acknowledgment()), (0x12, 0));
            let negotiated = crate::ethernet::parse_tcp_syn_options(reply.segment());
            assert_eq!(negotiated.mss, Some(3940));
            assert!(!negotiated.sack_permitted);
            let our_seq = reply.sequence().wrapping_add(1);
            bridge.set_fast_path_mtu(1500);
            bridge.handshake_conns.get_mut(&key).unwrap().last_sent =
                StdInstant::now().checked_sub(Duration::from_secs(1));
            assert_eq!(bridge.poll_handshakes(), first);
            let payload = b"payload in the final IPv6 handshake ACK";
            let mut ack = guest_frame(key, our_seq, &[], payload);
            ack[66..70].copy_from_slice(&0u32.to_be_bytes());
            ack[75] = 0x18;
            seal(&mut ack);
            rejects_malformed(&mut bridge, &ack);
            ack.extend_from_slice(&[0xee; 12]);
            let replies = bridge.try_complete_handshake(&ack).unwrap();
            assert_eq!(replies.len(), 1);
            assert_eq!(
                checked_reply(&replies[0], key).acknowledgment(),
                payload.len() as u32
            );
            assert_eq!(bridge.handshake_count(), 0);
            assert_eq!(bridge.fast_path_count(), 1);
            assert!(bridge.try_fast_path_intercept(&ack).is_some());
            let conn = &bridge.fast_path_conns[&key];
            assert_eq!(conn.peer_mss, if options.is_empty() { 1220 } else { 900 });
            assert_eq!(
                conn.guest_window.load(Ordering::Relaxed),
                if options.is_empty() { 1234 } else { 1234 << 2 }
            );
            assert_eq!(conn.up_bytes, payload.len() as u64);
            conn.stream.shutdown(Shutdown::Write).unwrap();
            let mut received = Vec::new();
            server.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, payload);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn active_ipv6_handshake_preserves_mss_and_retransmission() {
    for (mtu, expected) in [(1500, 1440), (4000, 3940), (0, 1), (usize::MAX, 65475)] {
        for options in [&[][..], &[2, 4, 3, 132][..]] {
            tokio::time::timeout(TIMEOUT, async {
                let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let (_server, _) = listener.accept().await.unwrap();
                let key = key(listener.local_addr().unwrap().port());
                let mut bridge = TcpBridge::new(GW_IP);
                bridge.set_fast_path_mtu(mtu);
                bridge.initiate_active_handshake(
                    key,
                    client.into_std().unwrap(),
                    GW_MAC,
                    GUEST_MAC,
                );
                bridge.handshake_conns.get_mut(&key).unwrap().our_isn = u32::MAX;
                let first = bridge.poll_handshakes();
                assert_eq!(first.len(), 1);
                let syn = checked_reply(&first[0], key);
                assert_eq!(syn.flags(), 0x02);
                let advertised = crate::ethernet::parse_tcp_syn_options(syn.segment());
                assert_eq!(advertised.mss, Some(expected));
                assert!(!advertised.sack_permitted);
                bridge.set_fast_path_mtu(9000);
                bridge.handshake_conns.get_mut(&key).unwrap().last_sent =
                    StdInstant::now().checked_sub(Duration::from_secs(1));
                assert_eq!(bridge.poll_handshakes(), first);
                let mut reply = guest_frame(key, 0, options, &[]);
                reply[75] = 0x12;
                seal(&mut reply);
                rejects_malformed(&mut bridge, &reply);
                reply[70..74].copy_from_slice(&1u32.to_be_bytes());
                assert!(bridge.try_complete_handshake(&reply).is_none());
                assert_eq!(bridge.handshake_count(), 1);
                reply[70..74].copy_from_slice(&0u32.to_be_bytes());
                let frames = bridge.try_complete_handshake(&reply).unwrap();
                assert_eq!(frames.len(), 1);
                let ack = checked_reply(&frames[0], key);
                assert_eq!(
                    (ack.flags(), ack.sequence(), ack.acknowledgment()),
                    (0x10, 0, 0)
                );
                assert_eq!(bridge.handshake_count(), 0);
                assert_eq!(
                    bridge.fast_path_conns[&key].peer_mss,
                    if options.is_empty() { 1220 } else { 900 }
                );
            })
            .await
            .unwrap();
        }
    }
}
