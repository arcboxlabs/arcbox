use super::*;
use std::io::Read;
use std::net::Shutdown;

const ACCEPTED_ACK: u32 = 2004;

async fn pair_after_host_fin() -> (TcpBridge, SynFlowKey, std::net::TcpStream) {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40200,
        dst_ip: Ipv4Addr::new(198, 18, 30, 120),
        dst_port: 443,
    };
    let mut server = fast_path_pair(&mut bridge, key, 2000).await;
    server.shutdown(Shutdown::Write).unwrap();
    let fin = poll_until_frame(&mut bridge).await;
    assert_ne!(tcp_flags_of(&fin) & 0x01, 0);
    let ack = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2000,
        tcp_seq_of(&fin).wrapping_add(1),
        65535,
        0x10,
        &[],
    );
    bridge.try_fast_path_intercept(&ack);
    assert!(bridge.fast_path_conns[&key].host_eof);

    // A peer FIN closes downloads; uploads must remain usable.
    let segment = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2000,
        1001,
        65535,
        0x18,
        b"sent",
    );
    let reply = bridge.try_fast_path_intercept(&segment).unwrap();
    assert_eq!(tcp_ack_of(&reply), ACCEPTED_ACK);
    let mut received = [0_u8; 4];
    server.read_exact(&mut received).unwrap();
    assert_eq!(&received, b"sent");
    (bridge, key, server)
}

#[tokio::test]
async fn intercepted_broken_pipe_resets_without_acking_rejected_bytes() {
    let (mut bridge, key, _server) = pair_after_host_fin().await;
    // Local write shutdown injects EPIPE without a reset-delivery race.
    bridge.fast_path_conns[&key]
        .stream
        .shutdown(Shutdown::Write)
        .unwrap();
    let segment = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        ACCEPTED_ACK,
        1001,
        65535,
        0x18,
        &[0x5a; 64],
    );
    let reply = bridge.try_fast_path_intercept(&segment).unwrap();
    assert_eq!(tcp_ack_of(&reply), ACCEPTED_ACK);
    assert_ne!(
        tcp_flags_of(&reply) & 0x04,
        0,
        "write failure must emit RST"
    );
    assert_eq!(bridge.fast_path_count(), 0);
}

#[tokio::test]
async fn polled_broken_pipe_resets_without_acking_retained_bytes() {
    let (mut bridge, key, _server) = pair_after_host_fin().await;
    let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
    conn.buffer_upload_segment(ACCEPTED_ACK, b"first");
    conn.buffer_upload_segment(ACCEPTED_ACK + 5, b"next");
    // Local write shutdown injects EPIPE without a reset-delivery race.
    conn.stream.shutdown(Shutdown::Write).unwrap();

    let replies = bridge.poll_fast_path();
    assert_eq!(replies.len(), 1);
    assert_eq!(tcp_ack_of(&replies[0]), ACCEPTED_ACK);
    assert_ne!(
        tcp_flags_of(&replies[0]) & 0x04,
        0,
        "write failure must emit RST"
    );
    assert_eq!(bridge.fast_path_count(), 0);
}
