use super::*;
use arcbox_packet::ip::{IpPacketView, TcpPacketView};
use std::io::Read;

#[tokio::test]
async fn ipv6_fast_path_validates_frames_and_splices_declared_payload() {
    for bare_ip in [false, true] {
        let key = SynFlowKey {
            src_ip: "2001:db8::2".parse().unwrap(),
            src_port: 41100,
            dst_ip: "2001:db8::1".parse().unwrap(),
            dst_port: 443,
        };
        let base = u32::MAX - 100;
        let mut bridge = TcpBridge::new(GW_IP);
        let mut server = fast_path_pair(&mut bridge, key, base).await;
        let payload: Vec<u8> = (0..1031).map(|i| (i % 251) as u8).collect();
        let params = crate::ethernet::TcpFrameParams {
            src_ip: key.src_ip,
            dst_ip: key.dst_ip,
            src_port: key.src_port,
            dst_port: key.dst_port,
            seq: base,
            ack: 1000,
            window: 1234,
            src_mac: GUEST_MAC,
            dst_mac: GW_MAC,
        };
        let mut frame = crate::ethernet::build_tcp_data_frame(&params, &payload);
        frame.splice(54..54, [6, 0, 0, 0, 0, 0, 0, 0]);
        frame[20] = 0;
        let length = u16::try_from(frame.len() - 54).unwrap();
        frame[18..20].copy_from_slice(&length.to_be_bytes());
        let input = if bare_ip { &frame[14..] } else { &frame[..] };
        let offset = if bare_ip { 0 } else { 14 };
        let intercept = |bridge: &mut TcpBridge, bytes: &[u8]| {
            if bare_ip {
                bridge.try_fast_path_intercept_ip_batch(&[bytes])
            } else {
                bridge.try_fast_path_intercept_batch(&[bytes])
            }
        };
        for length in 0..input.len() {
            assert!(intercept(&mut bridge, &input[..length]).is_none());
        }
        for (index, value) in [(0, 0x40), (4, 0xff), (6, 17), (40, 44), (60, 0x40)] {
            let mut malformed = input.to_vec();
            malformed[offset + index] = value;
            assert!(intercept(&mut bridge, &malformed).is_none());
        }
        if !bare_ip {
            let mut wrong_family = frame.clone();
            wrong_family[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            assert!(intercept(&mut bridge, &wrong_family).is_none());
        }
        assert_eq!(bridge.fast_path_conns[&key].up_bytes, 0);
        assert_eq!(bridge.fast_path_conns[&key].last_ack, base);
        let mut padded = input.to_vec();
        padded.extend_from_slice(&[0xee; 31]);
        let (consumed, ack) = intercept(&mut bridge, &padded).unwrap();
        assert_eq!(consumed, 1);
        let tcp = TcpPacketView::parse(IpPacketView::parse(&ack[14..]).unwrap()).unwrap();
        assert_eq!(tcp.ip().source(), key.dst_ip);
        assert_eq!(tcp.ip().destination(), key.src_ip);
        assert_eq!(
            tcp.acknowledgment(),
            base.wrapping_add(payload.len() as u32)
        );
        let mut received = vec![0; payload.len()];
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, payload);
        assert_eq!(bridge.fast_path_conns[&key].up_bytes, payload.len() as u64);
        assert_eq!(
            bridge.fast_path_conns[&key]
                .guest_window
                .load(std::sync::atomic::Ordering::Relaxed),
            1234
        );
    }
}
