use super::super::{packet::tcp_at, upload_batch::UploadBatch};
use super::*;
use arcbox_packet::checksum;
use std::io::Read;

fn frames(key: SynFlowKey, payload: &[u8], size: usize, base: u32) -> Vec<Vec<u8>> {
    payload
        .chunks(size)
        .enumerate()
        .map(|(index, data)| {
            let mut frame = crate::ethernet::build_tcp_data_frame(
                &crate::ethernet::TcpFrameParams {
                    src_ip: key.src_ip,
                    dst_ip: key.dst_ip,
                    src_port: key.src_port,
                    dst_port: key.dst_port,
                    seq: base.wrapping_add((index * size) as u32),
                    ack: 1000,
                    window: 1234,
                    src_mac: GUEST_MAC,
                    dst_mac: GW_MAC,
                },
                data,
            );
            frame.splice(74..74, [1; 4]);
            frame[66] = 0x60;
            frame.splice(54..54, [6, 0, 0, 0, 0, 0, 0, 0]);
            frame[20] = 0;
            let length = u16::try_from(frame.len() - 54).unwrap();
            frame[18..20].copy_from_slice(&length.to_be_bytes());
            frame[78..80].fill(0);
            let sum = checksum::checksum_add(&frame[22..54])
                + (frame.len() - 62) as u32
                + 6
                + checksum::checksum_add(&frame[62..]);
            frame[78..80].copy_from_slice(&checksum::checksum_fold(sum).to_be_bytes());
            frame
        })
        .collect()
}

#[tokio::test]
async fn ipv6_upload_batches_preserve_boundaries_and_payload() {
    for ip_start in [0, ETH_HEADER_LEN] {
        let key = SynFlowKey {
            src_ip: "2001:db8::2".parse().unwrap(),
            src_port: 41102,
            dst_ip: "2001:db8::1".parse().unwrap(),
            dst_port: 443,
        };
        let base = u32::MAX - 100;
        let payload: Vec<_> = (0..65 * 257).map(|i| (i % 251) as u8).collect();
        let frames = frames(key, &payload, 257, base);
        let input: Vec<_> = frames
            .iter()
            .map(|frame| &frame[ETH_HEADER_LEN - ip_start..])
            .collect();
        let first_payload = tcp_at(input[0], ip_start).unwrap().payload();
        let mut invalid: Vec<_> = (0..input[1].len())
            .map(|len| input[1][..len].to_vec())
            .collect();
        for offset in (8..40).chain([48, 50, 55, 59, 62, 61, 6, 40]) {
            let mut frame = input[1].to_vec();
            frame[ip_start + offset] ^= 1;
            invalid.push(frame);
        }
        for frame in invalid {
            let candidates = [frame];
            let mut batch = UploadBatch::new(first_payload);
            batch.extend(
                &tcp_at(input[0], ip_start).unwrap(),
                &candidates,
                base.wrapping_add(257),
                ip_start,
            );
            assert_eq!(batch.count, 1);
        }
        let large = self::frames(key, &vec![7; 280_000], 40_000, base);
        let large: Vec<_> = large
            .iter()
            .map(|frame| &frame[ETH_HEADER_LEN - ip_start..])
            .collect();
        let mut batch = UploadBatch::new(tcp_at(large[0], ip_start).unwrap().payload());
        batch.extend(
            &tcp_at(large[0], ip_start).unwrap(),
            &large[1..],
            base.wrapping_add(40_000),
            ip_start,
        );
        assert_eq!(batch.count, 6);
        let mut bridge = TcpBridge::new(GW_IP);
        let mut server = fast_path_pair(&mut bridge, key, base).await;
        let intercept = |bridge: &mut TcpBridge, frames: &[&[u8]]| {
            if ip_start == 0 {
                bridge.try_fast_path_intercept_ip_batch(frames)
            } else {
                bridge.try_fast_path_intercept_batch(frames)
            }
        };
        let (count, ack) = intercept(&mut bridge, &input).unwrap();
        assert_eq!(count, 64);
        assert_eq!(
            tcp_at(&ack, ETH_HEADER_LEN).unwrap().acknowledgment(),
            base.wrapping_add(64 * 257)
        );
        let (count, ack) = intercept(&mut bridge, &input[64..]).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            tcp_at(&ack, ETH_HEADER_LEN).unwrap().acknowledgment(),
            base.wrapping_add(payload.len() as u32)
        );
        let mut received = vec![0; payload.len()];
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, payload);
        assert_eq!(bridge.fast_path_conns[&key].up_bytes, payload.len() as u64);
    }
}
