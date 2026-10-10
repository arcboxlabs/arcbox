use super::*;

fn assert_handshake_mss(frame: &[u8], expected: u16) {
    let tcp = ETH_HEADER_LEN + 20;
    let options = crate::ethernet::parse_tcp_syn_options(&frame[tcp..]);
    assert_eq!(options.mss, Some(expected));
    assert!(!options.sack_permitted);
    assert_eq!(
        checksum::tcp_checksum(
            frame[26..30].try_into().unwrap(),
            frame[30..34].try_into().unwrap(),
            &frame[tcp..],
        ),
        0,
    );
}

#[tokio::test]
async fn passive_handshake_advertises_link_mss_and_retains_peer_mss() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_mtu(4000);
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40300,
        dst_ip: Ipv4Addr::LOCALHOST,
        dst_port: port,
    };
    let syn = make_guest_syn_frame(key.src_port, key.dst_ip, port, 100, &[2, 4, 5, 180]);
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    let (_server, _) = listener.accept().await.unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let frames = bridge.poll_handshakes();
            if !frames.is_empty() {
                break frames;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first.len(), 1);
    assert_handshake_mss(&first[0], 3960);
    let our_isn = bridge.handshake_conns[&key].our_isn;
    bridge.set_fast_path_mtu(1500);
    bridge.handshake_conns.get_mut(&key).unwrap().last_sent =
        StdInstant::now().checked_sub(std::time::Duration::from_secs(1));
    assert_eq!(
        bridge.poll_handshakes(),
        first,
        "retransmission must preserve the negotiated options"
    );
    let ack = make_guest_ack_frame(key.src_port, key.dst_ip, port, 101, our_isn.wrapping_add(1));
    assert!(bridge.try_complete_handshake(&ack).is_some());
    assert_eq!(bridge.fast_path_conns[&key].peer_mss, 1460);
}

#[tokio::test]
async fn active_handshake_advertises_bounded_link_mss_without_changing_peer_fallback() {
    for (mtu, expected) in [(1500, 1460), (4000, 3960), (0, 1), (usize::MAX, 65495)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.set_fast_path_mtu(mtu);
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 8080,
            dst_ip: GW_IP,
            dst_port: 61500,
        };
        bridge.initiate_active_handshake(key, client.into_std().unwrap(), GW_MAC, GUEST_MAC);
        let first = bridge.poll_handshakes();
        assert_eq!(first.len(), 1);
        assert_handshake_mss(&first[0], expected);
        let our_isn = bridge.handshake_conns[&key].our_isn;
        bridge.set_fast_path_mtu(9000);
        bridge.handshake_conns.get_mut(&key).unwrap().last_sent =
            StdInstant::now().checked_sub(std::time::Duration::from_secs(1));
        assert_eq!(bridge.poll_handshakes(), first);
        let reply = make_guest_syn_ack_frame(8080, GW_IP, 61500, 100, our_isn.wrapping_add(1));
        assert!(bridge.try_complete_handshake(&reply).is_some());
        assert_eq!(bridge.fast_path_conns[&key].peer_mss, 1460);
    }
}
