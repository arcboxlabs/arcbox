use super::*;

async fn limited_fast_path_pair(
    bridge: &mut TcpBridge,
    key: SynFlowKey,
    base: u32,
) -> (std::net::TcpStream, usize) {
    // Constrain the handshake window; shrinking buffers after connect leaves
    // the peer's previously advertised window available to the first write.
    let listener = tokio::net::TcpSocket::new_v4().unwrap();
    listener.set_recv_buffer_size(4096).unwrap();
    listener.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = listener.listen(1).unwrap();
    let client = tokio::net::TcpSocket::new_v4().unwrap();
    client.set_send_buffer_size(4096).unwrap();
    let connect = client.connect(listener.local_addr().unwrap());
    let (client, accepted) = tokio::join!(connect, listener.accept());
    let client = client.unwrap().into_std().unwrap();
    let server = accepted.unwrap().0.into_std().unwrap();
    // Connecting can resize buffers; retain the limits after negotiation too.
    let send_size = {
        let sock = socket2::SockRef::from(&client);
        sock.set_send_buffer_size(4096).unwrap();
        sock.send_buffer_size().unwrap()
    };
    let recv_size = {
        let sock = socket2::SockRef::from(&server);
        sock.set_recv_buffer_size(4096).unwrap();
        sock.recv_buffer_size().unwrap()
    };
    bridge.promote_to_fast_path(key, client, fast_path_init(1000, base, 1460, None));
    server.set_nonblocking(false).unwrap();
    server
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    (server, guaranteed_oversized(send_size, recv_size))
}

/// Filling one upload hole must drain retained bytes only to the next hole,
/// including when the first segment crosses the sequence-number boundary.
#[tokio::test]
async fn upload_multi_hole_burst_advances_to_each_missing_range() {
    check_upload_multi_hole_burst(1460).await;
}

#[tokio::test]
async fn jumbo_upload_multi_hole_burst_preserves_bytes_and_ack_cursor() {
    check_upload_multi_hole_burst(3960).await;
}

async fn check_upload_multi_hole_burst(segment_size: usize) {
    use std::io::Read;
    let holes = [0, 16 * segment_size, 899 * segment_size];
    let payload: Vec<_> = (0..(3 * 1024 * 1024).max(901 * segment_size))
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    let base = u32::MAX - 1000;
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40040,
        dst_ip: Ipv4Addr::new(198, 18, 30, 108),
        dst_port: 443,
    };
    let mut server = fast_path_pair(&mut bridge, key, base).await;
    let reader = std::thread::spawn(move || {
        let mut received = Vec::new();
        server.read_to_end(&mut received).unwrap();
        received
    });
    let segment = |offset: usize, flags: u8, bytes: &[u8]| {
        make_guest_segment(
            (key.src_port, key.dst_ip, key.dst_port),
            base.wrapping_add(offset as u32),
            1000,
            65535,
            flags,
            bytes,
        )
    };

    for (index, bytes) in payload.chunks(segment_size).enumerate() {
        let offset = index * segment_size;
        if holes.contains(&offset) {
            continue;
        }
        let reply = bridge
            .try_fast_path_intercept(&segment(offset, 0x18, bytes))
            .unwrap();
        assert_eq!(tcp_ack_of(&reply), base, "the first hole must stay unACKed");
    }
    assert_eq!(
        bridge.fast_path_conns[&key].ooo_bytes,
        payload.len() - holes.len() * segment_size
    );

    for (index, &offset) in holes.iter().enumerate() {
        let next_hole = holes.get(index + 1).copied().unwrap_or(payload.len());
        let reply = bridge
            .try_fast_path_intercept(&segment(
                offset,
                0x18,
                &payload[offset..offset + segment_size],
            ))
            .unwrap();
        let mut cursor = tcp_ack_of(&reply).wrapping_sub(base) as usize;
        assert!(cursor >= offset && cursor <= next_hole);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while cursor < next_hole {
                for frame in bridge.poll_fast_path() {
                    let acked = tcp_ack_of(&frame).wrapping_sub(base) as usize;
                    assert!(acked >= cursor && acked <= next_hole);
                    cursor = acked;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("poll must drain retained bytes up to the next missing range");
        assert_eq!(cursor, next_hole);
        assert_eq!(bridge.poll_fast_path(), Vec::<Vec<u8>>::new());
    }

    assert!(bridge.fast_path_conns[&key].ooo_segs.is_empty());
    assert_eq!(bridge.fast_path_conns[&key].up_ooo_dropped, 0);
    let reply = bridge
        .try_fast_path_intercept(&segment(payload.len(), 0x11, &[]))
        .unwrap();
    assert_eq!(
        tcp_ack_of(&reply),
        base.wrapping_add(payload.len() as u32 + 1)
    );
    assert_eq!(reader.join().unwrap(), payload);
}

/// A partially written parked segment must stay before later arrivals,
/// including when the ACK cursor crosses the sequence-number boundary.
#[tokio::test]
async fn upload_ooo_insert_preserves_partially_written_front() {
    use std::io::Read;

    for base in [2000u32, u32::MAX - 1000] {
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
        let dst_ip = Ipv4Addr::new(198, 18, 30, 104);
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 40036,
            dst_ip,
            dst_port: 443,
        };
        let (mut server, oversized) = limited_fast_path_pair(&mut bridge, key, base).await;
        const HOLE: usize = 100;
        const LATER: usize = 100;
        let parked_end = HOLE + oversized;
        let payload: Vec<_> = (0..parked_end + LATER).map(|i| (i % 251) as u8).collect();
        let segment = |offset: usize, data: &[u8]| {
            make_guest_segment(
                (key.src_port, dst_ip, key.dst_port),
                base.wrapping_add(offset as u32),
                1000,
                65535,
                0x18,
                data,
            )
        };
        let parked = segment(HOLE, &payload[HOLE..parked_end]);
        bridge.try_fast_path_intercept(&parked).unwrap();
        let fill = segment(0, &payload[..HOLE]);
        let reply = bridge.try_fast_path_intercept(&fill).unwrap();
        let first_advance = tcp_ack_of(&reply).wrapping_sub(base) as usize;
        assert!(first_advance > HOLE && first_advance < parked_end);
        assert!(bridge.fast_path_conns[&key].up_short_writes > 0);

        let later = segment(parked_end, &payload[parked_end..]);
        let reply = bridge.try_fast_path_intercept(&later).unwrap();
        assert_eq!(
            tcp_ack_of(&reply).wrapping_sub(base) as usize,
            first_advance
        );
        assert_eq!(
            bridge.fast_path_conns[&key].ooo_segs.front().unwrap().0,
            base.wrapping_add(HOLE as u32),
            "the unwritten front must precede a future segment"
        );

        let mut received = Vec::new();
        let mut cursor = first_advance;
        for _ in 0..2000 {
            let mut bytes = vec![0; cursor - received.len()];
            server.read_exact(&mut bytes).unwrap();
            received.extend(bytes);
            if cursor == payload.len() {
                break;
            }
            let retry = segment(cursor, &payload[cursor..=cursor]);
            let reply = bridge.try_fast_path_intercept(&retry).unwrap();
            cursor = tcp_ack_of(&reply).wrapping_sub(base) as usize;
            assert!(cursor <= payload.len());
        }
        assert_eq!(cursor, payload.len(), "the retained tail must drain");
        assert_eq!(received, payload);
        assert!(bridge.fast_path_conns[&key].ooo_segs.is_empty());
    }
}

/// Retained upload bytes must resume when the host socket drains, without
/// waiting for the guest to retransmit bytes the bridge already holds.
#[tokio::test]
async fn poll_drains_upload_after_host_backpressure() {
    use tokio::io::AsyncReadExt;

    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let dst_ip = Ipv4Addr::new(198, 18, 30, 105);
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40037,
        dst_ip,
        dst_port: 443,
    };
    let base = 2000u32;
    let (server, oversized) = limited_fast_path_pair(&mut bridge, key, base).await;
    server.set_nonblocking(true).unwrap();
    let mut server = tokio::net::TcpStream::from_std(server).unwrap();
    const HOLE: usize = 100;
    let total = HOLE + oversized;
    let payload: Vec<_> = (0..total).map(|i| (i % 251) as u8).collect();
    let parked = make_guest_segment(
        (key.src_port, dst_ip, key.dst_port),
        base + HOLE as u32,
        1000,
        65535,
        0x18,
        &payload[HOLE..],
    );
    bridge.try_fast_path_intercept(&parked).unwrap();
    assert!(
        bridge.poll_fast_path().is_empty(),
        "a gap must remain unACKed"
    );
    let fill = make_guest_segment(
        (key.src_port, dst_ip, key.dst_port),
        base,
        1000,
        65535,
        0x18,
        &payload[..HOLE],
    );
    let reply = bridge.try_fast_path_intercept(&fill).unwrap();
    let mut cursor = tcp_ack_of(&reply).wrapping_sub(base) as usize;
    assert!(cursor > HOLE && cursor < total);
    let mut received = Vec::new();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let mut bytes = vec![0; cursor - received.len()];
            server.read_exact(&mut bytes).await.unwrap();
            received.extend(bytes);
            if cursor == total {
                break;
            }
            for frame in bridge.poll_fast_path() {
                let acked = tcp_ack_of(&frame).wrapping_sub(base) as usize;
                assert!(acked >= cursor && acked <= total);
                cursor = acked;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("poll must retry retained bytes after the host socket drains");

    assert_eq!(received, payload);
    assert!(bridge.fast_path_conns[&key].ooo_segs.is_empty());
}

/// Both a nonzero short write and a completely blocked in-order write
/// retain their tails for poll-driven recovery.
#[tokio::test]
async fn poll_recovers_in_order_upload_tails() {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    for prefill in [false, true] {
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 40038,
            dst_ip: Ipv4Addr::new(198, 18, 30, 106),
            dst_port: 443,
        };
        let base = 2000u32;
        let (server, oversized) = limited_fast_path_pair(&mut bridge, key, base).await;
        server.set_nonblocking(true).unwrap();
        let mut server = tokio::net::TcpStream::from_std(server).unwrap();
        let mut expected = Vec::new();
        let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
        if prefill {
            // Fill until the socket itself reports backpressure.
            while expected.len() < OOO_REASSEMBLY_CAP {
                match conn.stream.write(&[0xA5; 4096]) {
                    Ok(0) => panic!("host socket closed while filling its buffer"),
                    Ok(n) => expected.extend(std::iter::repeat_n(0xA5, n)),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    other => panic!("cannot fill the host socket: {other:?}"),
                }
            }
            assert!(
                expected.len() < OOO_REASSEMBLY_CAP,
                "the host socket must block"
            );
            conn.set_last_ack(base + expected.len() as u32);
        }
        let mut cursor = if prefill {
            loop {
                assert!(
                    expected.len() < OOO_REASSEMBLY_CAP,
                    "the host socket must block"
                );
                let before = expected.len();
                expected.push((before % 251) as u8);
                let segment = make_guest_segment(
                    (key.src_port, key.dst_ip, key.dst_port),
                    base + before as u32,
                    1000,
                    65535,
                    0x18,
                    &expected[before..],
                );
                let reply = bridge.try_fast_path_intercept(&segment).unwrap();
                let acked = tcp_ack_of(&reply).wrapping_sub(base) as usize;
                if acked == before {
                    assert!(bridge.fast_path_conns[&key].up_would_block > 0);
                    break acked;
                }
                assert_eq!(acked, expected.len());
            }
        } else {
            loop {
                assert!(
                    expected.len() + oversized <= OOO_REASSEMBLY_CAP,
                    "the host socket must produce a short write"
                );
                let before = expected.len();
                expected.extend((before..before + oversized).map(|i| (i % 251) as u8));
                let segment = make_guest_segment(
                    (key.src_port, key.dst_ip, key.dst_port),
                    base + before as u32,
                    1000,
                    65535,
                    0x18,
                    &expected[before..],
                );
                let reply = bridge.try_fast_path_intercept(&segment).unwrap();
                let cursor = tcp_ack_of(&reply).wrapping_sub(base) as usize;
                assert!(cursor >= before && cursor <= expected.len());
                if cursor < expected.len() {
                    assert!(cursor > before, "WouldBlock is not a nonzero short write");
                    assert!(bridge.fast_path_conns[&key].up_short_writes > 0);
                    break cursor;
                }
            }
        };
        let mut received = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let mut bytes = vec![0; cursor - received.len()];
                server.read_exact(&mut bytes).await.unwrap();
                received.extend(bytes);
                if cursor == expected.len() {
                    break;
                }
                for frame in bridge.poll_fast_path() {
                    let acked = tcp_ack_of(&frame).wrapping_sub(base) as usize;
                    assert!(acked >= cursor && acked <= expected.len());
                    cursor = acked;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("poll must deliver the in-order tail without retransmission");
        assert_eq!(received, expected);
        assert!(bridge.fast_path_conns[&key].ooo_segs.is_empty());
    }
}

/// A full reassembly queue must reserve room for the contiguous tail by
/// evicting the farthest future segment, without advancing the ACK.
#[tokio::test]
async fn upload_contiguous_tail_takes_priority_at_both_caps() {
    for byte_cap in [false, true] {
        let mut bridge = TcpBridge::new(GW_IP);
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 40039,
            dst_ip: Ipv4Addr::new(198, 18, 30, 107),
            dst_port: 443,
        };
        let _server = fast_path_pair(&mut bridge, key, 2000).await;
        let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
        // Preserve a partially written front before the current ACK cursor.
        conn.ooo_segs.push_back((1999, vec![0x11; 2]));
        conn.ooo_bytes = 2;
        let segment_len = if byte_cap { 2048 } else { 1 };
        while conn.ooo_bytes < OOO_REASSEMBLY_CAP && conn.ooo_segs.len() < OOO_MAX_SEGMENTS {
            let len = segment_len.min(OOO_REASSEMBLY_CAP - conn.ooo_bytes);
            let seq = 2000 + conn.ooo_bytes as u32;
            conn.ooo_segs.push_back((seq, vec![0x22; len]));
            conn.ooo_bytes += len;
        }
        if byte_cap {
            assert_eq!(conn.ooo_bytes, OOO_REASSEMBLY_CAP);
        } else {
            assert_eq!(conn.ooo_segs.len(), OOO_MAX_SEGMENTS);
        }
        let farthest = conn.ooo_segs.back().unwrap().0;
        conn.buffer_upload_segment(2000, &[0x33; 16]);
        assert_eq!(conn.last_ack, 2000);
        assert_eq!(conn.ooo_segs.front().unwrap().0, 1999);
        assert_eq!(conn.ooo_segs[1], (2000, vec![0x33; 16]));
        assert!(conn.ooo_segs.back().unwrap().0 < farthest);
        assert!(conn.up_ooo_dropped > 0);
        assert!(conn.ooo_segs.len() <= OOO_MAX_SEGMENTS);
        assert!(conn.ooo_bytes <= OOO_REASSEMBLY_CAP);
        assert_eq!(
            conn.ooo_bytes,
            conn.ooo_segs.iter().map(|(_, p)| p.len()).sum()
        );
    }
}
