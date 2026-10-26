use super::*;
use crate::egress::{AbandonHandle, EgressConn, EgressOutcome};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn controlled_pair(
    bridge: &mut TcpBridge,
    port: u16,
) -> (tokio::net::TcpStream, AbandonHandle, SynFlowKey) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let (transport, handle) = EgressConn::controlled_tcp(client.unwrap());
    let tx = super::egress::register_with_options(bridge, port, &[2, 4, 0x05, 0xb4]);
    assert!(tx.send(EgressOutcome::Connect(transport)).is_ok());
    let syn_ack = bridge.poll_handshakes().pop().unwrap();
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: port,
        dst_ip: Ipv4Addr::new(203, 0, 113, 1).into(),
        dst_port: 443,
    };
    let ack = packet(key, 101, tcp_seq_of(&syn_ack).wrapping_add(1), 0x10, &[]);
    assert!(bridge.try_complete_handshake(&ack).unwrap().is_empty());
    (accepted.unwrap().0, handle, key)
}

fn packet(key: SynFlowKey, seq: u32, ack: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        seq,
        ack,
        65535,
        flags,
        payload,
    )
}

fn check_ack(bridge: &mut TcpBridge, frame: &[u8], seq: u32, ack: u32) {
    let reply = bridge.try_fast_path_intercept(frame).unwrap();
    assert_eq!(tcp_flags_of(&reply), 0x10);
    assert_eq!(tcp_seq_of(&reply), seq);
    assert_eq!(tcp_ack_of(&reply), ack);
    assert_eq!(tcp_payload_len(&reply), 0);
    assert_eq!(u16::from_be_bytes(reply[48..50].try_into().unwrap()), 65535);
}

#[tokio::test]
async fn abandon_releases_transport_and_acks_payload_retransmission_and_fin() {
    let sink = std::sync::Arc::new(CaptureSink(std::sync::Mutex::new(None)));
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_conn_sink(sink.clone());
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let (mut server, handle, key) = controlled_pair(&mut bridge, 40001).await;
    assert!(
        sink.0.lock().unwrap().is_none(),
        "controlled flow must stay polled"
    );
    let our_seq = bridge.fast_path_conns[&key]
        .our_seq
        .load(std::sync::atomic::Ordering::Relaxed);
    server.write_all(b"must not reach guest").await.unwrap();
    handle.abandon();
    assert!(bridge.poll_fast_path().is_empty());
    assert_eq!(bridge.fast_path_count(), 0);
    assert!(bridge.flow_controls.is_empty());
    let closed = tokio::time::timeout(std::time::Duration::from_secs(1), server.read_u8())
        .await
        .unwrap();
    assert!(closed.is_err(), "abandon must release the host fd");

    // A future segment must not ACK a gap. Retransmission repairs discarded future bytes.
    check_ack(
        &mut bridge,
        &packet(key, 105, our_seq, 0x18, b"later"),
        our_seq,
        101,
    );
    let first = packet(key, 101, our_seq, 0x18, b"data");
    let mut fragment = first.clone();
    fragment[20..22].copy_from_slice(&0x2000_u16.to_be_bytes());
    assert!(bridge.try_fast_path_intercept(&fragment).is_none());
    check_ack(&mut bridge, &first, our_seq, 105);
    check_ack(&mut bridge, &first, our_seq, 105);
    check_ack(
        &mut bridge,
        &packet(key, 105, our_seq, 0x18, b"later"),
        our_seq,
        110,
    );
    let fin = packet(key, 110, our_seq, 0x11, b"end");
    check_ack(&mut bridge, &fin, our_seq, 114);
    check_ack(&mut bridge, &fin, our_seq, 114);
    assert!(
        bridge
            .try_fast_path_intercept(&packet(key, 114, our_seq, 0x10, &[]))
            .unwrap()
            .is_empty()
    );
    assert!(
        bridge.poll_fast_path().is_empty(),
        "client FIN must not trigger our FIN"
    );
    let syn = make_guest_syn_frame(key.src_port, key.dst_ip, key.dst_port, 100, &[]);
    assert_eq!(
        tcp_flags_of(&bridge.handle_outbound_syn(&syn, GW_MAC, GUEST_MAC).unwrap()),
        0x10
    );
    assert_eq!(bridge.handshake_count(), 0);
    assert!(
        bridge
            .try_fast_path_intercept(&packet(key, 114, our_seq, 0x14, &[]))
            .unwrap()
            .is_empty()
    );
    assert!(bridge.abandoned_conns.is_empty());
}

#[tokio::test]
async fn abandon_wins_when_eof_or_reset_is_observed_before_the_final_signal_check() {
    for reset in [false, true] {
        let mut bridge = TcpBridge::new(GW_IP);
        let (server, handle, key) = controlled_pair(&mut bridge, 40001).await;
        let our_seq = bridge.fast_path_conns[&key]
            .our_seq
            .load(std::sync::atomic::Ordering::Relaxed);
        // Stage the actual poll checkpoint to control the I/O-to-signal interleaving.
        let controlled = bridge.prepare_controlled_poll();
        if reset {
            socket2::SockRef::from(&server)
                .set_linger(Some(std::time::Duration::ZERO))
                .unwrap();
        }
        drop(server);
        let mut frames = Vec::new();
        let deadline = StdInstant::now() + std::time::Duration::from_secs(1);
        while frames.is_empty() {
            assert!(StdInstant::now() < deadline);
            frames = bridge.poll_fast_path();
            tokio::task::yield_now().await;
        }
        assert!(tcp_flags_of(&frames[0]) & if reset { 0x04 } else { 0x01 } != 0);
        handle.abandon();
        bridge.finish_controlled_poll(controlled, &mut frames);
        assert!(
            frames.is_empty(),
            "post-I/O abandonment must suppress close frames"
        );
        assert_eq!(bridge.fast_path_count(), 0);
        check_ack(
            &mut bridge,
            &packet(key, 101, our_seq, 0x18, b"tail"),
            our_seq,
            105,
        );
    }
}

#[tokio::test]
async fn abandoned_flow_answers_keepalive_but_not_ordinary_ack() {
    let mut bridge = TcpBridge::new(GW_IP);
    let (_server, handle, key) = controlled_pair(&mut bridge, 40001).await;
    let our_seq = bridge.fast_path_conns[&key]
        .our_seq
        .load(std::sync::atomic::Ordering::Relaxed);
    handle.abandon();
    assert!(bridge.poll_fast_path().is_empty());
    check_ack(
        &mut bridge,
        &packet(key, 100, our_seq, 0x10, &[]),
        our_seq,
        101,
    );
    assert!(
        bridge
            .try_fast_path_intercept(&packet(key, 101, our_seq, 0x10, &[]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(bridge.abandoned_conns.len(), 1);
    assert!(bridge.poll_fast_path().is_empty());
}

#[tokio::test]
async fn dropping_control_preserves_ordinary_eof() {
    let mut bridge = TcpBridge::new(GW_IP);
    let (server, handle, _) = controlled_pair(&mut bridge, 40001).await;
    drop(handle);
    drop(server);
    let deadline = StdInstant::now() + std::time::Duration::from_secs(1);
    loop {
        assert!(StdInstant::now() < deadline);
        let frames = bridge.poll_fast_path();
        if let Some(frame) = frames.first() {
            assert_eq!(tcp_flags_of(frame), 0x11);
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(bridge.abandoned_conns.is_empty());
}

#[tokio::test]
async fn discard_states_expire_and_evict_without_frames() {
    use super::super::abandon::{ABANDON_IDLE_TIMEOUT, MAX_ABANDONED_CONNS};
    let mut bridge = TcpBridge::new(GW_IP);
    let (_server, handle, key) = controlled_pair(&mut bridge, 40001).await;
    handle.abandon();
    assert!(bridge.poll_fast_path().is_empty());
    let mut old = bridge.abandoned_conns[&key].clone();
    old.last_activity -= std::time::Duration::from_secs(1);
    for port in 1..=MAX_ABANDONED_CONNS {
        bridge.abandoned_conns.insert(
            SynFlowKey {
                src_port: port as u16,
                ..key
            },
            old.clone(),
        );
    }
    bridge.abandoned_conns.remove(&key);
    let (_server, handle, _) = controlled_pair(&mut bridge, 40001).await;
    handle.abandon();
    assert!(bridge.poll_fast_path().is_empty());
    assert_eq!(bridge.abandoned_conns.len(), MAX_ABANDONED_CONNS);
    assert!(
        bridge.abandoned_conns.contains_key(&key),
        "new state must survive capacity eviction"
    );
    for state in bridge.abandoned_conns.values_mut() {
        state.last_activity -= ABANDON_IDLE_TIMEOUT;
    }
    assert!(bridge.poll_fast_path().is_empty());
    assert!(bridge.abandoned_conns.is_empty());
    assert!(
        bridge
            .try_fast_path_intercept(&packet(key, 101, 0, 0x18, b"expired"))
            .is_none()
    );
}

#[tokio::test]
async fn capacity_eviction_does_not_restore_a_raced_close_frame() {
    use super::super::abandon::{AbandonedConn, MAX_ABANDONED_CONNS};
    let mut bridge = TcpBridge::new(GW_IP);
    let (server_a, handle_a, key_a) = controlled_pair(&mut bridge, 40001).await;
    let (server_b, handle_b, key_b) = controlled_pair(&mut bridge, 40002).await;
    let controlled = bridge.prepare_controlled_poll();
    let seed = AbandonedConn::from(&bridge.fast_path_conns[&key_a]);
    for port in 1..MAX_ABANDONED_CONNS {
        bridge.abandoned_conns.insert(
            SynFlowKey {
                src_port: port as u16,
                ..key_a
            },
            seed.clone(),
        );
    }
    drop((server_a, server_b));
    let mut frames = Vec::new();
    let deadline = StdInstant::now() + std::time::Duration::from_secs(1);
    while frames.len() < 2 {
        assert!(StdInstant::now() < deadline);
        frames.extend(bridge.poll_fast_path());
        tokio::task::yield_now().await;
    }
    handle_a.abandon();
    handle_b.abandon();
    bridge.finish_controlled_poll(controlled, &mut frames);
    assert!(frames.is_empty());
    assert_eq!(bridge.abandoned_conns.len(), MAX_ABANDONED_CONNS);
    assert_ne!(
        bridge.abandoned_conns.contains_key(&key_a),
        bridge.abandoned_conns.contains_key(&key_b)
    );
}

#[tokio::test]
async fn managed_ipv6_abandonment_releases_owner_and_keeps_acknowledging_guest_data() {
    use crate::NativeTcp;
    use crate::ethernet::{
        SynParams, TcpFrameParams, build_tcp_ack_frame, build_tcp_data_frame, build_tcp_syn_frame,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let (client, mut server) = super::ownership::socket_pair().await;
    let owner = Arc::new(());
    let released = Arc::downgrade(&owner);
    let cancellation = CancellationToken::new();
    let (transport, handle) = EgressConn::controlled_tcp(NativeTcp::with_lifecycle(
        client,
        owner,
        cancellation.clone(),
    ));
    let key = SynFlowKey {
        src_ip: "2001:db8::2".parse().unwrap(),
        src_port: 40001,
        dst_ip: "2001:db8::1".parse().unwrap(),
        dst_port: 443,
    };
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let syn = build_tcp_syn_frame(&SynParams {
        src_ip: key.src_ip,
        dst_ip: key.dst_ip,
        src_port: key.src_port,
        dst_port: key.dst_port,
        seq: 100,
        src_mac: GUEST_MAC,
        dst_mac: GW_MAC,
        mss: 1440,
        wscale: None,
        sack_permitted: false,
    });
    let tx = super::egress::register_syn(&mut bridge, &syn);
    assert!(tx.send(EgressOutcome::Connect(transport)).is_ok());
    let syn_ack = bridge.poll_handshakes().pop().unwrap();
    let our_seq = super::super::packet::tcp_at(&syn_ack, ETH_HEADER_LEN)
        .unwrap()
        .sequence()
        .wrapping_add(1);
    let params = TcpFrameParams {
        src_ip: key.src_ip,
        dst_ip: key.dst_ip,
        src_port: key.src_port,
        dst_port: key.dst_port,
        seq: 101,
        ack: our_seq,
        window: 65535,
        src_mac: GUEST_MAC,
        dst_mac: GW_MAC,
    };
    assert!(
        bridge
            .try_complete_handshake(&build_tcp_ack_frame(&params))
            .unwrap()
            .is_empty()
    );
    assert!(released.upgrade().is_some());
    handle.abandon();
    cancellation.cancel();
    assert!(bridge.poll_fast_path().is_empty());
    assert!(released.upgrade().is_none());
    assert_eq!(bridge.fast_path_count(), 0);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), server.read_u8())
            .await
            .unwrap()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    let upload = build_tcp_data_frame(&params, b"tail");
    let (consumed, reply) = bridge
        .try_fast_path_intercept_ip_batch(&[&upload[ETH_HEADER_LEN..]])
        .unwrap();
    assert_eq!(consumed, 1);
    let tcp = super::super::packet::tcp_at(&reply, ETH_HEADER_LEN).unwrap();
    assert_eq!(tcp.ip().source(), key.dst_ip);
    assert_eq!(tcp.ip().destination(), key.src_ip);
    assert_eq!(tcp.flags(), 0x10);
    assert_eq!(tcp.sequence(), our_seq);
    assert_eq!(tcp.acknowledgment(), 105);
    assert!(tcp.payload().is_empty());
    bridge.abort(|_| panic!("an abandoned flow must not produce a reset"));
    assert!(bridge.abandoned_conns.is_empty());
}
