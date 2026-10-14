use super::*;
use crate::classifier::FrameClassifier;
use arcbox_packet::checksum;
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

const GUEST_ISN: u32 = 1000;
const GUEST_SEQ: u32 = GUEST_ISN + 1;
const TIMEOUT: Duration = Duration::from_secs(2);

struct PendingFlow {
    bridge: TcpBridge,
    classifier: FrameClassifier,
    upstream: tokio::net::TcpStream,
    key: SynFlowKey,
    our_seq: u32,
}

impl PendingFlow {
    async fn active() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (stream, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 8080,
            dst_ip: GW_IP,
            dst_port: 61500,
        };
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
        bridge.initiate_active_handshake(
            key,
            stream.unwrap().into_std().unwrap(),
            GW_MAC,
            GUEST_MAC,
        );
        let syn = bridge.poll_handshakes();
        assert_eq!(syn.len(), 1);
        Self {
            our_seq: tcp_seq_of(&syn[0]).wrapping_add(1),
            bridge,
            classifier: FrameClassifier::with_pool_capacity(GW_IP, 1500, 4),
            upstream: accepted.unwrap().0,
            key,
        }
    }

    async fn new() -> Self {
        Self::with_scale(2).await
    }

    async fn with_scale(scale: u8) -> Self {
        tokio::time::timeout(TIMEOUT, async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let key = SynFlowKey {
                src_ip: GUEST_IP,
                src_port: 40400,
                dst_ip: Ipv4Addr::LOCALHOST,
                dst_port: listener.local_addr().unwrap().port(),
            };
            let mut bridge = TcpBridge::new(GW_IP);
            bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
            let wake = Arc::new(Notify::new());
            bridge.set_handshake_waker(Arc::clone(&wake));
            let mut classifier = FrameClassifier::with_pool_capacity(GW_IP, 1500, 4);
            let mut syn = make_guest_syn_frame(
                key.src_port,
                key.dst_ip,
                key.dst_port,
                GUEST_ISN,
                &[2, 4, 0x05, 0xB4, 1, 3, 3, scale],
            );
            classify_guest_frame(&mut classifier, &mut syn);
            let gated = classifier.take_gated_syns();
            assert_eq!(gated.len(), 1);
            assert!(
                bridge
                    .handle_outbound_syn(&gated[0].frame, GW_MAC, GUEST_MAC)
                    .is_none()
            );
            let (upstream, _) = listener.accept().await.unwrap();
            wake.notified().await;
            let replies = bridge.poll_handshakes();
            assert_eq!(replies.len(), 1);
            assert_eq!(tcp_flags_of(&replies[0]), 0x12);
            let tcp = ETH_HEADER_LEN + 20;
            let our_isn = u32::from_be_bytes(replies[0][tcp + 4..tcp + 8].try_into().unwrap());
            Self {
                bridge,
                classifier,
                upstream,
                key,
                our_seq: our_isn.wrapping_add(1),
            }
        })
        .await
        .expect("the loopback dial must produce a SYN-ACK")
    }

    fn queue(&mut self, seq: u32, flags: u8, window: u16, payload: &[u8]) {
        let mut frame = make_guest_segment(
            (self.key.src_port, self.key.dst_ip, self.key.dst_port),
            seq,
            self.our_seq,
            window,
            flags,
            payload,
        );
        classify_guest_frame(&mut self.classifier, &mut frame);
    }

    fn assert_upload_ack(&self, replies: &[Vec<u8>], expected_len: usize) {
        let conn = &self.bridge.fast_path_conns[&self.key];
        assert_eq!(conn.up_bytes, expected_len as u64);
        let accepted_end = GUEST_SEQ.wrapping_add(u32::try_from(conn.up_bytes).unwrap());
        assert_eq!(conn.last_ack, accepted_end);
        assert_eq!(
            conn.last_ack_shared
                .as_ref()
                .unwrap()
                .load(Ordering::Relaxed),
            accepted_end
        );
        assert_eq!(replies.len(), 1, "accepted payload must produce an ACK");
        assert_eq!(tcp_ack_of(&replies[0]), accepted_end);
    }

    async fn assert_received(&mut self, expected: &[u8]) {
        self.bridge.fast_path_conns[&self.key]
            .stream
            .shutdown(Shutdown::Write)
            .unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(TIMEOUT, self.upstream.read_to_end(&mut received))
            .await
            .expect("the upstream must reach EOF")
            .unwrap();
        assert_eq!(
            received, expected,
            "the upstream must receive each byte once"
        );
    }
}

fn classify_guest_frame(classifier: &mut FrameClassifier, frame: &mut [u8]) {
    let ip = &mut frame[ETH_HEADER_LEN..];
    assert_eq!(usize::from(u16::from_be_bytes([ip[2], ip[3]])), ip.len());
    assert!(ip.len() <= 1500);
    let header_len = usize::from(ip[0] & 0x0F) * 4;
    let src = ip[12..16].try_into().unwrap();
    let dst = ip[16..20].try_into().unwrap();
    let ip_checksum = checksum::ipv4_header_checksum(&ip[..header_len]);
    ip[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
    let tcp_checksum = checksum::tcp_checksum(src, dst, &ip[header_len..]);
    ip[header_len + 16..header_len + 18].copy_from_slice(&tcp_checksum.to_be_bytes());
    assert_eq!(checksum::ipv4_header_checksum(&ip[..header_len]), 0);
    assert_eq!(checksum::tcp_checksum(src, dst, &ip[header_len..]), 0);
    classifier.classify_ip_packet(ip, &mut None);
}

#[tokio::test]
async fn passive_open_payload_reaches_upstream_without_retransmission() {
    for inline in [false, true] {
        let mut flow = PendingFlow::new().await;
        let sink = Arc::new(CaptureSink(std::sync::Mutex::new(None)));
        if inline {
            flow.bridge.set_conn_sink(Arc::clone(&sink) as _);
        }
        let payload = b"payload carried by the third handshake packet";
        flow.queue(GUEST_SEQ, 0x18, 65535, payload);
        let replies = flow.classifier.drain_tcp(&mut flow.bridge);
        assert_eq!(flow.bridge.handshake_count(), 0);
        assert_eq!(flow.bridge.fast_path_count(), 1);
        assert_eq!(flow.bridge.fast_path_conns[&flow.key].inline_owned, inline);
        flow.assert_received(payload).await;
        flow.assert_upload_ack(&replies, payload.len());
        if inline {
            let promoted = sink.0.lock().unwrap().take().expect("inline connection");
            assert_eq!(
                promoted.last_ack.load(Ordering::Relaxed),
                GUEST_SEQ + payload.len() as u32
            );
        }
    }
}

#[tokio::test]
async fn pure_ack_applies_zero_window_and_payload_restores_scaled_window() {
    let mut flow = PendingFlow::new().await;
    let payload = b"data following a separate handshake ACK";
    flow.queue(GUEST_SEQ, 0x10, 0, &[]);
    assert_eq!(
        flow.classifier.drain_tcp(&mut flow.bridge),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        flow.bridge.fast_path_conns[&flow.key]
            .guest_window
            .load(Ordering::Relaxed),
        0
    );
    flow.queue(GUEST_SEQ, 0x18, 1234, payload);
    let replies = flow.classifier.drain_tcp(&mut flow.bridge);
    assert_eq!(
        flow.bridge.fast_path_conns[&flow.key]
            .guest_window
            .load(Ordering::Relaxed),
        1234 << 2
    );
    flow.assert_received(payload).await;
    flow.assert_upload_ack(&replies, payload.len());
}

#[tokio::test]
async fn handshake_drain_delivers_completion_payload_and_keeps_established_data() {
    let mut flow = PendingFlow::new().await;
    let payload = b"first part; second part";
    let split = 12;
    flow.queue(GUEST_SEQ, 0x18, 65535, &payload[..split]);
    let replies = flow
        .classifier
        .drain_handshake(|frame| flow.bridge.try_complete_handshake(frame));
    flow.assert_upload_ack(&replies, split);
    flow.queue(GUEST_SEQ + split as u32, 0x18, 65535, &payload[split..]);
    assert_eq!(
        flow.classifier
            .drain_handshake(|frame| flow.bridge.try_complete_handshake(frame)),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        flow.bridge.fast_path_conns[&flow.key].up_bytes,
        split as u64
    );
    let replies = flow.classifier.drain_tcp(&mut flow.bridge);
    flow.assert_received(payload).await;
    flow.assert_upload_ack(&replies, payload.len());
}

#[tokio::test]
async fn passive_open_payload_waits_for_missing_prefix() {
    let mut flow = PendingFlow::new().await;
    let payload = b"missing prefix; parked suffix";
    let split = 16;
    flow.queue(GUEST_SEQ + split as u32, 0x18, 65535, &payload[split..]);
    let replies = flow.classifier.drain_tcp(&mut flow.bridge);
    flow.assert_upload_ack(&replies, 0);
    let mut byte = [0];
    assert!(matches!(
        flow.upstream.try_read(&mut byte),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    flow.queue(GUEST_SEQ, 0x18, 65535, &payload[..split]);
    let replies = flow.classifier.drain_tcp(&mut flow.bridge);
    flow.assert_received(payload).await;
    flow.assert_upload_ack(&replies, payload.len());
}

struct WindowSink {
    expected: u32,
    captured: std::sync::Mutex<Option<crate::direct_rx::PromotedConn>>,
}

impl crate::direct_rx::ConnSink for WindowSink {
    fn send_conn(&self, conn: crate::direct_rx::PromotedConn) -> bool {
        // Observe publication from another thread before promotion can return to the ACK handler.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert_eq!(conn.guest_window.load(Ordering::Relaxed), self.expected);
                    assert_eq!(
                        send_budget(
                            conn.our_seq.load(Ordering::Relaxed),
                            conn.guest_acked.load(Ordering::Relaxed),
                            conn.guest_window.load(Ordering::Relaxed),
                        ),
                        self.expected
                    );
                })
                .join()
                .unwrap();
        });
        *self.captured.lock().unwrap() = Some(conn);
        true
    }
}

fn assert_window_publication_and_reopen(
    flow: &mut PendingFlow,
    mut completion: Vec<u8>,
    expected: u32,
    scale: u8,
) {
    let sink = Arc::new(WindowSink {
        expected,
        captured: std::sync::Mutex::new(None),
    });
    flow.bridge.set_conn_sink(Arc::clone(&sink) as _);
    classify_guest_frame(&mut flow.classifier, &mut completion);
    flow.classifier.drain_tcp(&mut flow.bridge);
    let promoted = sink.captured.lock().unwrap().take().unwrap();
    assert!(flow.bridge.fast_path_conns[&flow.key].inline_owned);
    assert_eq!(promoted.guest_window.load(Ordering::Relaxed), expected);
    flow.queue(GUEST_SEQ, 0x10, 1234, &[]);
    flow.classifier.drain_tcp(&mut flow.bridge);
    assert_eq!(promoted.guest_window.load(Ordering::Relaxed), 1234 << scale);
    assert_eq!(
        send_budget(
            promoted.our_seq.load(Ordering::Relaxed),
            promoted.guest_acked.load(Ordering::Relaxed),
            promoted.guest_window.load(Ordering::Relaxed),
        ),
        1234 << scale
    );
}

#[tokio::test]
async fn passive_initial_window_is_scaled_before_publication() {
    for (offered, scale) in [(2, 2), (14, 14), (255, 14)] {
        for window in [0, 7] {
            let mut flow = PendingFlow::with_scale(offered).await;
            let completion = make_guest_segment(
                (flow.key.src_port, flow.key.dst_ip, flow.key.dst_port),
                GUEST_SEQ,
                flow.our_seq,
                window,
                0x10,
                &[],
            );
            assert_window_publication_and_reopen(
                &mut flow,
                completion,
                u32::from(window) << scale,
                scale,
            );
        }
    }
}

#[tokio::test]
async fn active_initial_window_is_unscaled_before_publication() {
    for scale in [None, Some(2), Some(14), Some(255)] {
        for window in [0u16, 7] {
            let mut flow = PendingFlow::active().await;
            let mut options = vec![2, 4, 0x05, 0xB4];
            if let Some(scale) = scale {
                options.extend_from_slice(&[1, 3, 3, scale]);
            }
            let mut completion = make_guest_syn_frame(
                flow.key.src_port,
                flow.key.dst_ip,
                flow.key.dst_port,
                GUEST_ISN,
                &options,
            );
            let tcp = ETH_HEADER_LEN + 20;
            completion[tcp + 13] = 0x12;
            completion[tcp + 8..tcp + 12].copy_from_slice(&flow.our_seq.to_be_bytes());
            completion[tcp + 14..tcp + 16].copy_from_slice(&window.to_be_bytes());
            assert_window_publication_and_reopen(
                &mut flow,
                completion,
                u32::from(window),
                scale.unwrap_or(0).min(14),
            );
        }
    }
}
