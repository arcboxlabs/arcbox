//! Retransmissions of more than one frame's worth of in-flight data must be
//! split exactly like first transmissions, on both the polled and the inline
//! loss-recovery paths.

use super::*;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;

/// More than the 64 KiB an IPv4 total-length field can describe.
const IN_FLIGHT: usize = 160 * 1024;
/// Guest window wide enough for `IN_FLIGHT`: 65535 << 2.
const WSCALE: u8 = 2;
const SCALED_WINDOW: u32 = 65535 << WSCALE;

fn large_window_init(our_seq: u32, last_ack: u32, peer_mss: u16) -> FastPathInit {
    FastPathInit {
        our_seq,
        last_ack,
        peer_mss,
        peer_wscale: Some(WSCALE),
        guest_window: SCALED_WINDOW,
    }
}

/// Asserts that `frames` are data frames that each fit the large-frame
/// payload budget and together re-send `[start, start + len)` once, in order.
fn assert_covers_in_bounded_frames(frames: &[Vec<u8>], start: u32, len: usize) {
    assert!(frames.len() > 1, "{len} bytes need more than one frame");
    let mut next = start;
    for frame in frames {
        let ip_len = usize::from(u16::from_be_bytes([
            frame[ETH_HEADER_LEN + 2],
            frame[ETH_HEADER_LEN + 3],
        ]));
        assert_eq!(
            frame.len(),
            ETH_HEADER_LEN + ip_len,
            "IP length matches frame"
        );
        let payload = tcp_payload_len(frame);
        assert!(
            (1..=super::super::LARGE_FRAME_MAX_PAYLOAD).contains(&payload),
            "frame payload {payload} exceeds the large-frame budget"
        );
        assert_eq!(tcp_seq_of(frame), next, "frames are contiguous");
        next = next.wrapping_add(payload as u32);
    }
    assert_eq!(
        next.wrapping_sub(start) as usize,
        len,
        "all in-flight bytes resent"
    );
}

/// Promotes a polled (non-inline) large-frame flow and puts `IN_FLIGHT`
/// download bytes in flight with no guest ACK. The upstream stays open so no
/// FIN joins the retransmission.
async fn polled_flow_in_flight(
    src_port: u16,
    dst_ip: Ipv4Addr,
) -> (TcpBridge, SynFlowKey, tokio::net::TcpStream) {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.enable_large_frames();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let (mut server, _) = accepted.unwrap();
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port,
        dst_ip: dst_ip.into(),
        dst_port: 443,
    };
    bridge.promote_to_fast_path(
        key,
        client.unwrap().into_std().unwrap(),
        large_window_init(1, 1, 1460),
    );

    server.write_all(&vec![0x5A; IN_FLIGHT]).await.unwrap();
    let mut sent = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sent < IN_FLIGHT && std::time::Instant::now() < deadline {
        sent += bridge
            .poll_fast_path()
            .iter()
            .map(|frame| tcp_payload_len(frame))
            .sum::<usize>();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(sent, IN_FLIGHT, "initial transmission");
    (bridge, key, server)
}

#[tokio::test]
async fn polled_rto_retransmit_splits_large_in_flight() {
    let (mut bridge, key, _server) =
        polled_flow_in_flight(40300, Ipv4Addr::new(198, 18, 31, 1)).await;
    backdate_rto_clock(&mut bridge, &key);
    assert_covers_in_bounded_frames(&bridge.poll_fast_path(), 1, IN_FLIGHT);
}

#[tokio::test]
async fn polled_fast_retransmit_splits_large_in_flight() {
    let dst_ip = Ipv4Addr::new(198, 18, 31, 2);
    let (mut bridge, _key, _server) = polled_flow_in_flight(40301, dst_ip).await;
    for _ in 0..3 {
        let dup = make_guest_segment((40301, dst_ip, 443), 1, 1, 65535, 0x10, &[]);
        bridge.try_fast_path_intercept(&dup).expect("intercepted");
    }
    assert_covers_in_bounded_frames(&bridge.poll_fast_path(), 1, IN_FLIGHT);
}

/// Promotes an inline-owned large-frame flow and has the owner put
/// `IN_FLIGHT` bytes in flight through the shared retransmission ring.
async fn inline_flow_in_flight(
    src_port: u16,
    dst_ip: Ipv4Addr,
) -> (TcpBridge, crate::direct_rx::PromotedConn, SynFlowKey) {
    let sink = std::sync::Arc::new(CaptureSink(std::sync::Mutex::new(None)));
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.enable_large_frames();
    bridge.set_conn_sink(std::sync::Arc::clone(&sink) as _);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let _server = accepted.unwrap();
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port,
        dst_ip: dst_ip.into(),
        dst_port: 443,
    };
    bridge.promote_to_fast_path(
        key,
        client.unwrap().into_std().unwrap(),
        large_window_init(1000, 2000, 9000),
    );
    let promoted = sink.0.lock().unwrap().take().expect("inline conn");
    owner_sends(&promoted, &vec![0xA5; IN_FLIGHT]);
    assert_eq!(promoted.guest_window.load(Ordering::Relaxed), SCALED_WINDOW);
    (bridge, promoted, key)
}

#[tokio::test]
async fn inline_rto_retransmit_splits_large_in_flight() {
    let (mut bridge, _promoted, key) =
        inline_flow_in_flight(40302, Ipv4Addr::new(198, 18, 31, 3)).await;
    backdate_rto_clock(&mut bridge, &key);
    assert_covers_in_bounded_frames(&bridge.poll_fast_path(), 1000, IN_FLIGHT);
}

#[tokio::test]
async fn inline_fast_retransmit_splits_large_in_flight() {
    let dst_ip = Ipv4Addr::new(198, 18, 31, 4);
    let (mut bridge, _promoted, _key) = inline_flow_in_flight(40303, dst_ip).await;
    for _ in 0..3 {
        let dup = make_guest_segment((40303, dst_ip, 443), 2000, 1000, 65535, 0x10, &[]);
        bridge.try_fast_path_intercept(&dup);
    }
    assert_covers_in_bounded_frames(&bridge.poll_fast_path(), 1000, IN_FLIGHT);
}
