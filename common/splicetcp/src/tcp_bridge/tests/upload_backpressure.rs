use super::*;

async fn limited_fast_path_pair(
    bridge: &mut TcpBridge,
    key: SynFlowKey,
    base: u32,
) -> (std::net::TcpStream, usize) {
    let server = fast_path_pair(bridge, key, base).await;
    let send_size = {
        let sock = socket2::SockRef::from(&bridge.fast_path_conns[&key].stream);
        sock.set_send_buffer_size(4096).unwrap();
        sock.send_buffer_size().unwrap()
    };
    let recv_size = {
        let sock = socket2::SockRef::from(&server);
        sock.set_recv_buffer_size(4096).unwrap();
        sock.recv_buffer_size().unwrap()
    };
    (server, guaranteed_oversized(send_size, recv_size))
}

/// Retained upload bytes must resume when the host socket drains, without
/// waiting for the guest to retransmit bytes the bridge already holds.
#[tokio::test]
async fn poll_drains_upload_after_host_backpressure() {
    use std::io::Read;

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
    let (mut server, oversized) = limited_fast_path_pair(&mut bridge, key, base).await;
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
            server.read_exact(&mut bytes).unwrap();
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

/// Both an initial short write and a completely blocked in-order write
/// retain their tails for poll-driven recovery.
#[tokio::test]
async fn poll_recovers_in_order_upload_tails() {
    use std::io::{Read, Write};

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
        let (mut server, oversized) = limited_fast_path_pair(&mut bridge, key, base).await;
        let mut expected = Vec::new();
        let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
        if prefill {
            // The handshake may have advertised more room before the buffers shrank.
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
            expected.extend((0..oversized).map(|i| (i % 251) as u8));
            let segment = make_guest_segment(
                (key.src_port, key.dst_ip, key.dst_port),
                base,
                1000,
                65535,
                0x18,
                &expected,
            );
            let reply = bridge.try_fast_path_intercept(&segment).unwrap();
            let cursor = tcp_ack_of(&reply).wrapping_sub(base) as usize;
            assert!(cursor > 0 && cursor < expected.len());
            assert!(bridge.fast_path_conns[&key].up_short_writes > 0);
            cursor
        };
        let mut received = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let mut bytes = vec![0; cursor - received.len()];
                server.read_exact(&mut bytes).unwrap();
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
