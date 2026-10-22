//! Explicit abort releases pending, active, and established TCP owners.

use super::super::ownership::socket_pair;
use super::*;
use crate::{EgressConn, EgressResolver, FlowMeta, NativeTcp};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(2);

fn bridge() -> TcpBridge {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge
}

struct PendingResolver(Mutex<Option<oneshot::Receiver<Option<EgressConn>>>>);

impl EgressResolver for PendingResolver {
    fn resolve(&self, _flow: FlowMeta) -> oneshot::Receiver<Option<EgressConn>> {
        self.0.lock().unwrap().take().unwrap()
    }
}

#[tokio::test]
async fn abort_resolved_passive_releases_primary_before_reset() {
    for isn in [1000, u32::MAX] {
        let (client, mut peer) = socket_pair().await;
        let lease = Arc::new(());
        let released = Arc::downgrade(&lease);
        let (sender, receiver) = oneshot::channel();
        assert!(
            sender
                .send(Some(EgressConn::Tcp(NativeTcp::with_lifecycle(
                    client,
                    lease,
                    CancellationToken::new(),
                ))))
                .is_ok()
        );
        let mut bridge = bridge();
        bridge.set_egress_resolver(Arc::new(PendingResolver(Mutex::new(Some(receiver)))));
        let syn = make_guest_syn_frame(key().src_port, key().dst_ip, key().dst_port, 2000, &[]);
        assert!(
            bridge
                .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
                .is_none()
        );
        bridge.handshake_conns.get_mut(&key()).unwrap().our_isn = isn;
        assert_eq!(tcp_flags_of(&bridge.poll_handshakes()[0]), 0x12);
        let mut emitted = 0;
        bridge.abort(|frame| {
            assert!(released.upgrade().is_none());
            assert_reset(&frame, key(), (isn.wrapping_add(1), 0, 0x04));
            emitted += 1;
        });
        assert_eq!(emitted, 1);
        assert_eq!(
            tokio::time::timeout(TIMEOUT, peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
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
