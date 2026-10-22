use super::*;
use arcbox_packet::{
    checksum,
    ip::{IpPacketView, TcpPacketView},
};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn ipv6_polled_download_obeys_mtu_peer_mss_and_avoids_ipv4_offload() {
    for (mtu, peer_mss, large_frames, segment_limit) in [
        (1500, 9000, false, 1440),
        (4000, 1200, false, 1200),
        (1500, 9000, true, 1440),
        (1500, 0, false, 1),
    ] {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
        bridge.set_fast_path_mtu(mtu);
        if large_frames {
            bridge.enable_large_frames();
        }
        let sink = std::sync::Arc::new(CaptureSink(std::sync::Mutex::new(None)));
        bridge.set_conn_sink(sink.clone());
        let key = SynFlowKey {
            src_ip: "2001:db8::2".parse().unwrap(),
            src_port: 41000,
            dst_ip: "2001:db8::1".parse().unwrap(),
            dst_port: 443,
        };
        let initial_seq = u32::MAX - 5000;
        bridge.promote_to_fast_path(
            key,
            client.into_std().unwrap(),
            initial_seq,
            700,
            peer_mss,
            None,
        );
        assert!(
            sink.0.lock().unwrap().is_none(),
            "IPv6 must stay on the polled path"
        );
        assert!(!bridge.fast_path_conns[&key].inline_owned);
        let payload: Vec<u8> = (0..if peer_mss == 0 { 33 } else { 9001 })
            .map(|i| (i % 251) as u8)
            .collect();
        server.write_all(&payload).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut received = Vec::new();
            while received.len() < payload.len() {
                for frame in bridge.poll_fast_path() {
                    assert_eq!(&frame[12..14], &0x86ddu16.to_be_bytes());
                    let ip = IpPacketView::parse(&frame[ETH_HEADER_LEN..]).unwrap();
                    assert_eq!(ip.source(), key.dst_ip);
                    assert_eq!(ip.destination(), key.src_ip);
                    assert_eq!(ip.packet().len(), frame.len() - ETH_HEADER_LEN);
                    assert!(ip.packet().len() <= mtu);
                    let tcp = TcpPacketView::parse(ip).unwrap();
                    assert_eq!(tcp.source_port(), key.dst_port);
                    assert_eq!(tcp.destination_port(), key.src_port);
                    assert_eq!(
                        tcp.sequence(),
                        initial_seq.wrapping_add(received.len() as u32)
                    );
                    assert_eq!(tcp.acknowledgment(), 700);
                    assert!(!tcp.payload().is_empty() && tcp.payload().len() <= segment_limit);
                    let sum = checksum::checksum_add(&ip.packet()[8..40])
                        + tcp.segment().len() as u32
                        + 6
                        + checksum::checksum_add(tcp.segment());
                    assert_eq!(checksum::checksum_fold(sum), 0);
                    received.extend_from_slice(tcp.payload());
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(received, payload);
        })
        .await
        .unwrap();
        assert_eq!(
            bridge.fast_path_conns[&key]
                .our_seq
                .load(std::sync::atomic::Ordering::Relaxed),
            initial_seq.wrapping_add(payload.len() as u32)
        );
    }
}
