use super::*;
use std::io::{Read, Write};
use std::net::Shutdown;

fn key() -> SynFlowKey {
    SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40300,
        dst_ip: Ipv4Addr::new(198, 18, 31, 1),
        dst_port: 443,
    }
}

fn segments(base: u32, payload: &[u8], size: usize) -> Vec<Vec<u8>> {
    let key = key();
    payload
        .chunks(size)
        .enumerate()
        .map(|(index, data)| {
            make_guest_segment(
                (key.src_port, key.dst_ip, key.dst_port),
                base.wrapping_add((index * size) as u32),
                1000,
                65535,
                0x18,
                data,
            )
        })
        .collect()
}

#[tokio::test]
async fn upload_batch_wraps_sequence_and_acks_only_the_consumed_prefix() {
    let base = u32::MAX - 100;
    let payload: Vec<_> = (0..65 * 257).map(|i| (i % 251) as u8).collect();
    let mut bridge = TcpBridge::new(GW_IP);
    let mut server = fast_path_pair(&mut bridge, key(), base).await;
    let frames = segments(base, &payload, 257);
    let (count, ack) = bridge.try_fast_path_intercept_batch(&frames).unwrap();
    assert_eq!(count, 64);
    assert_eq!(tcp_ack_of(&ack), base.wrapping_add(64 * 257));
    let (count, ack) = bridge.try_fast_path_intercept_batch(&frames[64..]).unwrap();
    assert_eq!(count, 1);
    assert_eq!(tcp_ack_of(&ack), base.wrapping_add(payload.len() as u32));
    let mut got = vec![0; payload.len()];
    server.read_exact(&mut got).unwrap();
    assert_eq!(got, payload);
    let (_, ack) = bridge.try_fast_path_intercept_batch(&frames).unwrap();
    assert_eq!(tcp_ack_of(&ack), base.wrapping_add(payload.len() as u32));
    assert_eq!(
        bridge.fast_path_conns[&key()].up_bytes,
        payload.len() as u64
    );
}

#[test]
fn upload_batch_stops_at_flow_control_gaps_controls_and_malformed_frames() {
    use super::super::upload_batch::UploadBatch;
    let frames = segments(2000, &[7; 100], 50);
    let first = &frames[0];
    let candidate = &frames[1];
    let mut batch = UploadBatch::new(&first[54..]);
    batch.extend(first, &frames[1..], 2050, ETH_HEADER_LEN);
    assert_eq!(batch.count, 2);
    for (offset, value) in [
        (26, 99),
        (34, 99),
        (41, 99),
        (45, 99),
        (48, 1),
        (47, 0x19),
        (20, 0x20),
        (14, 0x44),
        (46, 0x40),
        (16, 0xff),
    ] {
        let mut changed = candidate.clone();
        changed[offset] = value;
        let mut batch = UploadBatch::new(&first[54..]);
        let candidates = [changed];
        batch.extend(first, &candidates, 2050, ETH_HEADER_LEN);
        assert_eq!(
            batch.count, 1,
            "changed offset {offset} must stop coalescing"
        );
    }
    for length in 0..candidate.len() {
        let mut batch = UploadBatch::new(&first[54..]);
        let candidates = [&candidate[..length]];
        batch.extend(first, &candidates, 2050, ETH_HEADER_LEN);
        assert_eq!(batch.count, 1);
    }
    let large = segments(2000, &vec![7; 280_000], 40_000);
    let mut batch = UploadBatch::new(&large[0][54..]);
    batch.extend(&large[0], &large[1..], 42_000, ETH_HEADER_LEN);
    assert_eq!(batch.count, 6, "batch payload must stay within 256 KiB");
}

#[tokio::test]
async fn borrowed_ip_batch_validates_headers_and_splices_only_payload() {
    use super::super::upload_batch::UploadBatch;

    let base = u32::MAX - 100;
    let payload: Vec<_> = (0..65 * 33).map(|i| (i % 251) as u8).collect();
    let mut bridge = TcpBridge::new(GW_IP);
    let mut server = fast_path_pair(&mut bridge, key(), base).await;
    let packets: Vec<_> = segments(base, &payload, 33)
        .into_iter()
        .map(|frame| {
            let mut ip = frame[ETH_HEADER_LEN..].to_vec();
            ip.splice(20..20, [1; 4]);
            ip[0] = 0x46;
            ip.splice(44..44, [1; 4]);
            ip[36] = 0x60;
            let len = u16::try_from(ip.len()).unwrap();
            ip[2..4].copy_from_slice(&len.to_be_bytes());
            ip
        })
        .collect();
    let mut invalid = Vec::new();
    for (offset, value) in [
        (0, 0x66),
        (0, 0x44),
        (0, 0x4f),
        (2, 0xff),
        (3, 39),
        (6, 0x20),
        (7, 1),
        (9, 17),
        (36, 0x40),
        (36, 0xf0),
        (37, 0x02),
        (37, 0x12),
    ] {
        let mut ip = packets[1].clone();
        ip[offset] = value;
        invalid.push(ip);
    }
    invalid.extend((0..packets[1].len()).map(|len| packets[1][..len].to_vec()));
    let mut oversized = packets[1].clone();
    oversized.resize(65_536, 0);
    invalid.push(oversized);
    for ip in invalid {
        let candidates = [ip];
        assert!(
            bridge
                .try_fast_path_intercept_ip_batch(&candidates)
                .is_none()
        );
        let mut batch = UploadBatch::new(&packets[0][48..]);
        batch.extend(&packets[0], &candidates, base.wrapping_add(33), 0);
        assert_eq!(
            batch.count, 1,
            "invalid following packets must stop coalescing"
        );
    }
    assert_eq!(bridge.fast_path_conns[&key()].last_ack, base);
    assert_eq!(bridge.fast_path_conns[&key()].up_bytes, 0);
    let (count, ack) = bridge.try_fast_path_intercept_ip_batch(&packets).unwrap();
    assert_eq!(count, 64);
    assert_eq!(tcp_ack_of(&ack), base.wrapping_add(64 * 33));
    let (count, ack) = bridge
        .try_fast_path_intercept_ip_batch(&packets[64..])
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(tcp_ack_of(&ack), base.wrapping_add(payload.len() as u32));
    let mut got = vec![0; payload.len()];
    server.read_exact(&mut got).unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
async fn upload_batch_backpressure_retains_suffix_until_poll_and_fin() {
    let base = u32::MAX - 1000;
    let mut bridge = TcpBridge::new(GW_IP);
    let mut server = fast_path_pair(&mut bridge, key(), base).await;
    let recv_size = socket2::SockRef::from(&server).recv_buffer_size().unwrap();
    socket2::SockRef::from(&server)
        .set_recv_buffer_size(4096)
        .unwrap();
    socket2::SockRef::from(&bridge.fast_path_conns[&key()].stream)
        .set_send_buffer_size(4096)
        .unwrap();
    // Fill both kernel queues without involving the bridge's retained upload queue.
    let mut prefix = 0;
    loop {
        match bridge
            .fast_path_conns
            .get_mut(&key())
            .unwrap()
            .stream
            .write(&[0x5a; 4096])
        {
            Ok(n) => {
                assert!(n > 0);
                prefix += n;
                assert!(prefix < 8 * 1024 * 1024);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("fill: {e}"),
        }
    }
    let payload: Vec<_> = (0..64 * 3960).map(|i| (i % 251) as u8).collect();
    let frames = segments(base, &payload, 3960);
    let packets: Vec<_> = frames
        .iter()
        .map(|frame| &frame[ETH_HEADER_LEN..])
        .collect();
    let (count, reply) = bridge.try_fast_path_intercept_ip_batch(&packets).unwrap();
    drop(packets);
    drop(frames);
    assert_eq!(count, 64);
    let written = tcp_ack_of(&reply).wrapping_sub(base) as usize;
    assert!(
        written < payload.len(),
        "the oversized batch must hit backpressure"
    );
    let conn = &bridge.fast_path_conns[&key()];
    let retained: Vec<_> = conn
        .ooo_segs
        .iter()
        .flat_map(|(seq, data)| {
            let overlap = conn.last_ack.wrapping_sub(*seq).cast_signed().max(0) as usize;
            data[overlap..].iter().copied()
        })
        .collect();
    assert_eq!(retained, payload[written..]);
    let fin = make_guest_segment(
        (key().src_port, key().dst_ip, key().dst_port),
        base.wrapping_add(payload.len() as u32),
        1000,
        65535,
        0x11,
        &[],
    );
    let deferred = bridge.try_fast_path_intercept(&fin).unwrap();
    assert_eq!(tcp_ack_of(&deferred), base.wrapping_add(written as u32));
    assert!(bridge.fast_path_conns[&key()].guest_fin_at.is_none());
    // Release the artificial receive limit after proving retained bytes and deferred FIN.
    socket2::SockRef::from(&server)
        .set_recv_buffer_size(recv_size)
        .unwrap();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        got
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while bridge.fast_path_conns[&key()].last_ack != base.wrapping_add(payload.len() as u32) {
            bridge.poll_fast_path();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        bridge.fast_path_conns[&key()].up_bytes,
        payload.len() as u64
    );
    assert!(bridge.fast_path_conns[&key()].ooo_segs.is_empty());
    let closed = bridge.try_fast_path_intercept(&fin).unwrap();
    assert_eq!(
        tcp_ack_of(&closed),
        base.wrapping_add(payload.len() as u32 + 1)
    );
    let got = reader.join().unwrap();
    assert_eq!(&got[..prefix], vec![0x5a; prefix]);
    assert_eq!(&got[prefix..], payload);
}

#[tokio::test]
async fn upload_batch_broken_pipe_resets_without_acking_the_batch() {
    let mut bridge = TcpBridge::new(GW_IP);
    let _server = fast_path_pair(&mut bridge, key(), 2000).await;
    bridge.fast_path_conns[&key()]
        .stream
        .shutdown(Shutdown::Write)
        .unwrap();
    let frames = segments(2000, &[7; 1024], 64);
    let (count, reply) = bridge.try_fast_path_intercept_batch(&frames).unwrap();
    assert_eq!(count, 16);
    assert_eq!(tcp_ack_of(&reply), 2000);
    assert_ne!(tcp_flags_of(&reply) & 0x04, 0);
    assert_eq!(bridge.fast_path_count(), 0);
}

#[tokio::test]
async fn retained_upload_batches_clip_overlaps_and_stop_at_wrapping_gap() {
    for (size, count) in [(257, 140), (40_000, 15)] {
        let base = u32::MAX - 100;
        let end = 5 + (count - 1) * (size / 2) + size;
        let payload: Vec<_> = (0..end + 48).map(|i| (i % 251) as u8).collect();
        let mut bridge = TcpBridge::new(GW_IP);
        let mut server = fast_path_pair(&mut bridge, key(), base).await;
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            server.read_to_end(&mut got).unwrap();
            got
        });
        let conn = bridge.fast_path_conns.get_mut(&key()).unwrap();
        for index in 0..count {
            let start = 5 + index * (size / 2);
            conn.buffer_upload_segment(
                base.wrapping_add(start as u32),
                &payload[start..start + size],
            );
            // Nested retransmissions must not overwrite bytes from an earlier segment.
            conn.buffer_upload_segment(base.wrapping_add(start as u32 + 1), &[0xff; 2]);
        }
        conn.buffer_upload_segment(base.wrapping_add(end as u32 + 17), &payload[end + 17..]);
        let prefix = segments(base, &payload[..10], 10);
        bridge.try_fast_path_intercept(&prefix[0]).unwrap();
        let target = base.wrapping_add(end as u32);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while bridge.fast_path_conns[&key()].last_ack != target {
                assert!(bridge.fast_path_conns[&key()].last_ack.wrapping_sub(base) < end as u32);
                bridge.poll_fast_path();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(bridge.poll_fast_path(), Vec::<Vec<u8>>::new());
        let conn = &bridge.fast_path_conns[&key()];
        assert_eq!(conn.ooo_segs.len(), 1);
        assert_eq!(conn.ooo_bytes, 31);
        assert_eq!(conn.up_bytes, end as u64);
        let gap = segments(target, &payload[end..end + 17], 17);
        bridge.try_fast_path_intercept(&gap[0]).unwrap();
        let conn = &bridge.fast_path_conns[&key()];
        assert_eq!(conn.last_ack, base.wrapping_add(payload.len() as u32));
        assert_eq!(conn.up_bytes, payload.len() as u64);
        assert_eq!(conn.ooo_bytes, 0);
        assert!(conn.ooo_segs.is_empty());
        conn.stream.shutdown(Shutdown::Write).unwrap();
        assert_eq!(reader.join().unwrap(), payload);
    }
}
