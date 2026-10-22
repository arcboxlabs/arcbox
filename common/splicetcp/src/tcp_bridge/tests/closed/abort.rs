//! Explicit abort releases pending, active, and established TCP owners.

use super::super::ownership::socket_pair;
use super::*;
use crate::direct_rx::{ConnSink, PromotedConn};
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

fn pending_handshake(receiver: oneshot::Receiver<Option<EgressConn>>, wake: bool) -> TcpBridge {
    let mut bridge = bridge();
    bridge.set_egress_resolver(Arc::new(PendingResolver(Mutex::new(Some(receiver)))));
    if wake {
        bridge.set_handshake_waker(Arc::new(tokio::sync::Notify::new()));
    }
    let syn = make_guest_syn_frame(key().src_port, key().dst_ip, key().dst_port, 2000, &[]);
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    bridge
}

#[derive(Default)]
struct HoldingSink(Mutex<Vec<PromotedConn>>);

impl ConnSink for HoldingSink {
    fn send_conn(&self, conn: PromotedConn) -> bool {
        self.0.lock().unwrap().push(conn);
        true
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
