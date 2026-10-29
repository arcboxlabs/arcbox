//! Explicit abort preserves TCP cursors and cancels every flow owner.

use super::*;
use crate::ethernet::{TcpFrameParams, build_tcp_data_frame};
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;

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

#[test]
fn abort_pending_passive_handshake_counts_original_syn_payload_and_fin() {
    for (seq, flags, expected_ack) in [
        (2000, 0x02, 2009),
        (2000, 0x03, 2010),
        (u32::MAX - 3, 0x02, 5),
        (u32::MAX - 3, 0x03, 6),
    ] {
        let (sender, receiver) = oneshot::channel();
        let mut bridge = bridge();
        bridge.set_egress_resolver(Arc::new(PendingResolver(Mutex::new(Some(receiver)))));
        let packet = guest_packet(key(), (seq, 0), flags, b"syn data");
        let mut frame = Vec::from(GW_MAC);
        frame.extend_from_slice(&GUEST_MAC);
        frame.extend_from_slice(&[0x08, 0x00]);
        frame.extend_from_slice(&packet);
        assert!(
            bridge
                .handle_outbound_syn(&frame, GW_MAC, GUEST_MAC)
                .is_none()
        );
        let mut resets = Vec::new();
        bridge.abort(|frame| resets.push(frame));
        assert_eq!(resets.len(), 1);
        assert_reset(&resets[0], key(), (0, expected_ack, 0x14));
        assert!(sender.is_closed());
        assert_eq!(bridge.handshake_count(), 0);
    }
}

#[tokio::test]
async fn abort_uses_send_next_and_closed_reply_uses_the_intermediate_peer_ack() {
    for (ipv6, guest_acked, send_next, peer_ack) in [
        (false, 1000, 1060, 1030),
        (true, 1000, 1060, 1030),
        (false, u32::MAX - 7, 17, 5),
        (true, u32::MAX - 7, 17, 5),
    ] {
        let mut key = key();
        if ipv6 {
            key.src_ip = "2001:db8::2".parse().unwrap();
            key.dst_ip = "2001:db8::1".parse().unwrap();
        }
        let (client, mut peer) = socket_pair().await;
        let mut bridge = bridge();
        let lease = Arc::new(());
        let released = Arc::downgrade(&lease);
        let last_ack = u32::MAX - 3;
        bridge.promote_to_fast_path(
            key,
            NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, CancellationToken::new()),
            guest_acked,
            last_ack,
            1460,
            None,
        );
        let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
        conn.buffer_upload_segment(last_ack.wrapping_add(4), b"upload after a gap");
        conn.retransmit_buf.extend(b"download awaiting an ACK");
        conn.our_seq.store(send_next, Ordering::Relaxed);
        peer.write_all(b"unread upstream bytes").await.unwrap();

        let mut resets = Vec::new();
        bridge.abort(|frame| {
            assert!(
                released.upgrade().is_none(),
                "release the primary before emitting"
            );
            resets.push(frame);
        });
        assert_eq!(resets.len(), 1);
        assert_reset(&resets[0], key, (send_next, 0, 0x04));
        assert_eq!(bridge.fast_path_count(), 0);
        assert!(bridge.poll_fast_path().is_empty());
        assert!(bridge.poll_handshakes().is_empty());
        bridge.abort(|_| panic!("an aborted flow must not emit twice"));
        drop(bridge);
        let challenge = guest_packet(key, (last_ack, peer_ack), 0x10, &[]);
        let reply = TcpBridge::closed_reply_ip(&challenge, GW_MAC, GUEST_MAC).unwrap();
        assert_reset(&reply, key, (peer_ack, 0, 0x04));
        assert!(released.upgrade().is_none());
        let mut received = Vec::new();
        let result = tokio::time::timeout(TIMEOUT, peer.read_to_end(&mut received))
            .await
            .expect("abort must close the upstream socket");
        if let Err(error) = result {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert!(
            received.is_empty(),
            "abort must not forward parked upload bytes"
        );
    }
}

#[tokio::test]
async fn abort_passive_handshakes_releases_pending_and_queued_dials() {
    for wake in [false, true] {
        for phase in 0..3 {
            let (sender, receiver) = oneshot::channel();
            let mut sender = Some(sender);
            let mut bridge = pending_handshake(receiver, wake);
            let mut released = None;
            let mut peer = None;
            if phase > 0 {
                let (client, server) = socket_pair().await;
                peer = Some(server);
                let lease = Arc::new(());
                released = Some(Arc::downgrade(&lease));
                assert!(
                    sender
                        .take()
                        .unwrap()
                        .send(Some(EgressConn::Tcp(NativeTcp::with_lifecycle(
                            client,
                            lease,
                            CancellationToken::new(),
                        ))))
                        .is_ok()
                );
            }
            if phase == 2 {
                tokio::time::timeout(TIMEOUT, async {
                    loop {
                        let replies = bridge.poll_handshakes();
                        if !replies.is_empty() {
                            assert_eq!(replies.len(), 1);
                            assert_eq!(tcp_flags_of(&replies[0]), 0x12);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the completed dial must produce a SYN-ACK");
            }
            let seq = if phase == 2 {
                bridge.handshake_conns[&key()].our_isn.wrapping_add(1)
            } else {
                0
            };
            let mut resets = Vec::new();
            bridge.abort(|frame| resets.push(frame));
            assert_eq!(resets.len(), 1);
            let expected = if phase == 2 {
                (seq, 0, 0x04)
            } else {
                (0, 2001, 0x14)
            };
            assert_reset(&resets[0], key(), expected);
            assert_eq!(bridge.handshake_count(), 0);
            assert_eq!(bridge.fast_path_count(), 0);
            if let Some(sender) = sender.as_mut() {
                tokio::time::timeout(TIMEOUT, sender.closed())
                    .await
                    .unwrap();
            }
            if let Some(released) = released {
                tokio::time::timeout(TIMEOUT, async {
                    while released.upgrade().is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("abort must release the queued native socket");
            }
            bridge.abort(|_| panic!("an aborted handshake must not emit twice"));
            drop(peer);
        }
    }
}

#[tokio::test]
async fn abort_active_handshakes_drops_the_socket_without_sending_a_reset() {
    for sent in [false, true] {
        let (client, mut peer) = socket_pair().await;
        let mut bridge = bridge();
        bridge.initiate_active_handshake(key(), client.into_std().unwrap(), GW_MAC, GUEST_MAC);
        bridge.handshake_conns.get_mut(&key()).unwrap().our_isn = u32::MAX;
        if sent {
            let replies = bridge.poll_handshakes();
            assert_eq!(replies.len(), 1);
            assert_eq!(tcp_flags_of(&replies[0]), 0x02);
        }
        let mut resets = Vec::new();
        bridge.abort(|frame| resets.push(frame));
        assert!(resets.is_empty());
        assert_eq!(bridge.handshake_count(), 0);
        assert!(bridge.poll_handshakes().is_empty());
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(TIMEOUT, peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}

#[derive(Default)]
struct CloseObserver(Mutex<Vec<(crate::egress::FlowKey, u64, u64)>>);

impl crate::egress::FlowObserver for CloseObserver {
    fn on_flow_close(&self, key: crate::egress::FlowKey, up: u64, down: u64) {
        self.0.lock().unwrap().push((key, up, down));
    }
}

#[tokio::test]
async fn abort_cancels_retained_readers_and_accounts_each_flow_once() {
    let holding = Arc::new(HoldingSink::default());
    let observer = Arc::new(CloseObserver::default());
    let generation = CancellationToken::new();
    let mut bridge = bridge();
    bridge.set_conn_sink(holding.clone());
    bridge.set_flow_observer(observer.clone());
    let mut leases = Vec::new();
    let mut peers = Vec::new();
    for index in 0..2 {
        let (client, mut peer) = socket_pair().await;
        let lease = Arc::new(());
        leases.push(Arc::downgrade(&lease));
        let mut key = key();
        key.src_port += index;
        bridge.promote_to_fast_path(
            key,
            NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, generation.clone()),
            1000,
            2000,
            1460,
            None,
        );
        let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
        conn.up_bytes = 3;
        conn.down_bytes = 2;
        conn.down_shared
            .as_ref()
            .unwrap()
            .store(5, Ordering::Relaxed);
        peer.write_all(b"must not be published by the old reader")
            .await
            .unwrap();
        peers.push(peer);
    }
    let readers = std::mem::take(&mut *holding.0.lock().unwrap());
    assert_eq!(readers.len(), 2);
    let mut emitted = 0;
    bridge.abort(|frame| {
        emitted += 1;
        let packet =
            TcpPacketView::parse(IpPacketView::parse(&frame[ETH_HEADER_LEN..]).unwrap()).unwrap();
        let reader = readers
            .iter()
            .find(|reader| reader.guest_port == packet.destination_port())
            .unwrap();
        assert!(reader.reader_cancel.is_cancelled());
        assert!(reader.dead.load(Ordering::Relaxed));
    });
    assert_eq!(emitted, 2);
    assert_eq!(bridge.fast_path_count(), 0);
    bridge.abort(|_| panic!("abort must not emit twice"));
    let mut closed = observer.0.lock().unwrap().clone();
    closed.sort_by_key(|(key, _, _)| key.src_port);
    assert_eq!(closed.len(), 2);
    for (index, (flow, up, down)) in closed.iter().enumerate() {
        assert_eq!(flow.src_port, key().src_port + index as u16);
        assert_eq!((*up, *down), (3, 7));
    }
    assert!(!generation.is_cancelled());
    #[cfg(feature = "tokio-frame-sink")]
    {
        let (sink, mut frames) = crate::direct_rx::TokioFrameConnSink::channel(1);
        for reader in readers {
            assert!(sink.send_conn(reader));
        }
        drop(sink);
        assert_eq!(
            tokio::time::timeout(TIMEOUT, frames.recv()).await.unwrap(),
            None
        );
    }
    #[cfg(not(feature = "tokio-frame-sink"))]
    drop(readers);
    assert!(leases.iter().all(|lease| lease.upgrade().is_none()));
    assert_eq!(observer.0.lock().unwrap().len(), 2);
    drop(peers);
}
