use super::*;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn shared_reads_preserve_each_flow_and_segment_by_mss() {
    check_download_batches(false).await;
}

#[tokio::test]
async fn large_frame_reads_stay_within_ipv4_and_read_limits() {
    check_download_batches(true).await;
}

async fn check_download_batches(large_frames: bool) {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.set_fast_path_mtu(4000);
    if large_frames {
        bridge.enable_large_frames();
    }
    let keys = [40210, 40211].map(|src_port| SynFlowKey {
        src_ip: GUEST_IP,
        src_port,
        dst_ip: Ipv4Addr::new(198, 18, 30, 150),
        dst_port: 443,
    });
    let payloads = [17, 131].map(|seed| {
        (0..196_631)
            .map(|i| ((i * seed + i / 251) % 256) as u8)
            .collect::<Vec<_>>()
    });
    let mut writers = Vec::new();
    for (key, payload) in keys.iter().zip(&payloads) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let (mut server, _) = accepted.unwrap();
        bridge.promote_to_fast_path(
            *key,
            client.unwrap().into_std().unwrap(),
            fast_path_init(1000, 2000, 1460, Some(2)),
        );
        let payload = payload.clone();
        writers.push(tokio::spawn(async move {
            server.write_all(&payload).await.unwrap();
            server
        }));
    }

    let mut covered = [0_usize; 2];
    let deadline = StdInstant::now() + std::time::Duration::from_secs(3);
    while covered.iter().zip(&payloads).any(|(&n, p)| n < p.len()) {
        assert!(
            StdInstant::now() < deadline,
            "downloads stalled: {covered:?}"
        );
        // A scaled window above 128 KiB exercises the read cap as well as MSS.
        let windows = [40_001_u16, 10_003];
        for (i, key) in keys.iter().enumerate() {
            let ack = make_guest_segment(
                (key.src_port, key.dst_ip, key.dst_port),
                2000,
                1000 + covered[i] as u32,
                windows[i],
                0x10,
                &[],
            );
            bridge.try_fast_path_intercept(&ack).expect("guest ACK");
        }
        let before = keys.map(|key| bridge.fast_path_conns[&key].our_seq.load(Ordering::Relaxed));
        let batch = bridge.poll_fast_path();
        let had_frames = !batch.is_empty();
        for frame in batch {
            let tcp = ETH_HEADER_LEN + 20;
            let port = u16::from_be_bytes(frame[tcp + 2..tcp + 4].try_into().unwrap());
            let i = keys.iter().position(|key| key.src_port == port).unwrap();
            let data = &frame[tcp + 20..];
            let offset = tcp_seq_of(&frame).wrapping_sub(1000) as usize;
            let end = offset + data.len();
            assert_ne!(data, []);
            assert!(offset <= covered[i], "sequence gap in flow {i}");
            assert_eq!(data, &payloads[i][offset..end], "flow {i} payload");
            covered[i] = covered[i].max(end);
            let ip_len = u16::from_be_bytes(frame[16..18].try_into().unwrap()) as usize;
            assert_eq!(ip_len, frame.len() - ETH_HEADER_LEN);
            assert!(data.len() <= if large_frames { 32 * 1024 } else { 1460 });
            if !large_frames {
                assert_eq!(
                    checksum::tcp_checksum(
                        keys[i].dst_ip.octets(),
                        GUEST_IP.octets(),
                        &frame[tcp..]
                    ),
                    0
                );
            }
        }
        for (i, key) in keys.iter().enumerate() {
            let conn = &bridge.fast_path_conns[key];
            let sent = conn.our_seq.load(Ordering::Relaxed);
            let read_limit = if large_frames { 32 * 1024 } else { 128 * 1024 };
            assert!(sent.wrapping_sub(before[i]) <= read_limit);
            let acked = conn.guest_acked.load(Ordering::Relaxed);
            assert!(sent.wrapping_sub(acked) <= u32::from(windows[i]) << 2);
            let offset = conn.retransmit_seq.wrapping_sub(1000) as usize;
            assert!(
                conn.retransmit_buf
                    .iter()
                    .copied()
                    .eq(payloads[i][offset..sent.wrapping_sub(1000) as usize]
                        .iter()
                        .copied())
            );
        }
        if !had_frames {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    for writer in writers {
        writer.await.unwrap();
    }
}

#[tokio::test]
async fn rejected_inline_flow_allocates_scratch_for_polling() {
    struct AcceptFirst(std::sync::Mutex<Option<crate::direct_rx::PromotedConn>>);
    impl crate::direct_rx::ConnSink for AcceptFirst {
        fn send_conn(&self, conn: crate::direct_rx::PromotedConn) -> bool {
            let mut accepted = self.0.lock().unwrap();
            if accepted.is_some() {
                return false;
            }
            *accepted = Some(conn);
            true
        }
    }
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_conn_sink(std::sync::Arc::new(AcceptFirst(std::sync::Mutex::new(
        None,
    ))));
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40212,
        dst_ip: Ipv4Addr::new(198, 18, 30, 151),
        dst_port: 443,
    };
    let _inline_server = fast_path_pair(&mut bridge, key, 2000).await;
    assert_eq!(bridge.fast_path_read_buf, Vec::<u8>::new());
    assert!(bridge.fast_path_conns[&key].inline_owned);

    let fallback_key = SynFlowKey {
        src_port: 40213,
        ..key
    };
    let mut server = fast_path_pair(&mut bridge, fallback_key, 2000).await;
    assert!(!bridge.fast_path_conns[&fallback_key].inline_owned);
    use std::io::Write;
    server.write_all(b"fallback").unwrap();
    let frame = poll_until_frame(&mut bridge).await;
    assert_eq!(&frame[ETH_HEADER_LEN + 40..], b"fallback");
}
