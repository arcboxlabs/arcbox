use super::*;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::time::{Duration, Instant};

fn assert_payload(frames: &[Vec<u8>], start: u32, expected: &[u8]) {
    let actual: Vec<_> = frames
        .iter()
        .flat_map(|frame| {
            let tcp = ETH_HEADER_LEN + usize::from(frame[ETH_HEADER_LEN] & 15) * 4;
            let payload = tcp + usize::from(frame[tcp + 12] >> 4) * 4;
            let seq = tcp_seq_of(frame);
            frame[payload..]
                .iter()
                .enumerate()
                .map(move |(offset, byte)| (seq.wrapping_add(offset as u32), *byte))
        })
        .collect();
    let expected: Vec<_> = expected
        .iter()
        .enumerate()
        .map(|(offset, byte)| (start.wrapping_add(offset as u32), *byte))
        .collect();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn host_eof_before_guest_fin_retains_download_payload_and_original_fin() {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40502,
        dst_ip: Ipv4Addr::new(198, 18, 34, 2).into(),
        dst_port: 443,
    };
    let mut server = fast_path_pair(&mut bridge, key, 2000).await;
    bridge.fast_path_conns.get_mut(&key).unwrap().rto = Duration::from_secs(60);
    let data: Vec<_> = (0..4096).map(|index| (index % 251) as u8).collect();
    server.write_all(&data).unwrap();
    server.shutdown(Shutdown::Write).unwrap();
    let mut initial = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !bridge.fast_path_conns[&key].host_eof {
            initial.extend(bridge.poll_fast_path());
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let end = 1000 + data.len() as u32;
    assert_payload(&initial, 1000, &data);
    let original_fins: Vec<_> = initial
        .iter()
        .filter(|frame| tcp_flags_of(frame) & 1 != 0)
        .collect();
    assert_eq!(original_fins.len(), 1);
    assert_eq!(tcp_seq_of(original_fins[0]), end);

    // The guest closes uploads while a download hole and our first FIN remain unacknowledged.
    let guest_fin = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2000,
        1100,
        65535,
        0x11,
        &[],
    );
    let reply = bridge.try_fast_path_intercept(&guest_fin).unwrap();
    assert_eq!(
        bridge.fast_path_count(),
        1,
        "guest FIN must not discard download recovery state"
    );
    assert_eq!(
        tcp_flags_of(&reply),
        0x10,
        "acknowledge the guest FIN without creating another FIN"
    );
    assert_eq!(tcp_ack_of(&reply), 2001);
    assert_eq!(bridge.fast_path_conns[&key].fin_seq, Some(end));
    assert_eq!(
        bridge.fast_path_conns[&key]
            .retransmit_buf
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        data
    );
    let mut byte = [0; 1];
    assert_eq!(
        server.read(&mut byte).unwrap(),
        0,
        "guest FIN must half-close upstream writes"
    );

    let duplicate = bridge.try_fast_path_intercept(&guest_fin).unwrap();
    assert_eq!(
        tcp_ack_of(&duplicate),
        2001,
        "a duplicate guest FIN cannot consume sequence space twice"
    );
    assert_eq!(tcp_flags_of(&duplicate), 0x10);
    assert!(bridge.poll_fast_path().is_empty());
    assert_eq!(
        bridge.fast_path_conns[&key]
            .retransmit_buf
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        data[100..]
    );

    let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
    conn.rto = Duration::from_millis(1);
    conn.last_progress = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let recovered = bridge.poll_fast_path();
    assert_payload(&recovered, 1100, &data[100..]);
    let repaired_fins: Vec<_> = recovered
        .iter()
        .filter(|frame| tcp_flags_of(frame) & 1 != 0)
        .collect();
    assert_eq!(repaired_fins.len(), 1);
    assert_eq!(tcp_seq_of(repaired_fins[0]), end);
    assert!(recovered.iter().all(|frame| tcp_flags_of(frame) & 4 == 0));
    assert_eq!(bridge.fast_path_count(), 1);

    // Payload ACKs release only payload; the original FIN needs its own cumulative ACK.
    let payload_ack = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2001,
        end,
        65535,
        0x10,
        &[],
    );
    assert!(
        bridge
            .try_fast_path_intercept(&payload_ack)
            .unwrap()
            .is_empty()
    );
    assert!(bridge.poll_fast_path().is_empty());
    assert!(bridge.fast_path_conns[&key].retransmit_buf.is_empty());
    let conn = bridge.fast_path_conns.get_mut(&key).unwrap();
    conn.rto = Duration::from_millis(1);
    conn.last_progress = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let fin_only = bridge.poll_fast_path();
    assert_eq!(fin_only.len(), 1);
    assert_eq!(tcp_flags_of(&fin_only[0]), 0x11);
    assert_eq!(tcp_seq_of(&fin_only[0]), end);
    let final_ack = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2001,
        end + 1,
        65535,
        0x10,
        &[],
    );
    assert!(
        bridge
            .try_fast_path_intercept(&final_ack)
            .unwrap()
            .is_empty()
    );
    assert!(bridge.poll_fast_path().is_empty());
    assert_eq!(bridge.fast_path_count(), 0);
}
