//! Cancelled managed flows release their sockets without uploading or resetting a reset.

use super::*;
use crate::NativeTcp;
use crate::ethernet::{TcpFrameParams, build_tcp_data_frame, build_tcp_rst_frame};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Action {
    Poll,
    Ethernet,
    BareIp,
    PeerReset,
}

#[tokio::test]
async fn cancelled_flows_release_without_uploading_or_replying_to_resets() {
    for (src_ip, dst_ip) in [
        (GUEST_IP, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))),
        (
            "2001:db8::2".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        ),
    ] {
        for action in [
            Action::Poll,
            Action::Ethernet,
            Action::BareIp,
            Action::PeerReset,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, accepted) = tokio::join!(
                tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept(),
            );
            let mut peer = accepted.unwrap().0;
            let lease = Arc::new(());
            let released = Arc::downgrade(&lease);
            let generation = CancellationToken::new();
            let stream = NativeTcp::with_lifecycle(
                client.unwrap().into_std().unwrap(),
                lease,
                generation.clone(),
            );
            let key = SynFlowKey {
                src_ip,
                src_port: 40400,
                dst_ip,
                dst_port: 443,
            };
            let mut bridge = TcpBridge::new(GW_IP);
            bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
            bridge.promote_to_fast_path(key, stream, 1000, 2000, 1460, None);
            generation.cancel();
            assert!(released.upgrade().is_some());
            let params = TcpFrameParams {
                src_ip,
                dst_ip,
                src_port: key.src_port,
                dst_port: key.dst_port,
                seq: 2000,
                ack: 1000,
                window: 65535,
                src_mac: GUEST_MAC,
                dst_mac: GW_MAC,
            };
            let frames = match action {
                Action::Poll => bridge.poll_fast_path(),
                Action::PeerReset => {
                    let reply = bridge
                        .try_fast_path_intercept(&build_tcp_rst_frame(&params))
                        .unwrap();
                    assert!(reply.is_empty());
                    Vec::new()
                }
                Action::Ethernet | Action::BareIp => {
                    let frame = build_tcp_data_frame(&params, b"must not reach the upstream");
                    let (consumed, reply) = if matches!(action, Action::BareIp) {
                        bridge.try_fast_path_intercept_ip_batch(&[&frame[ETH_HEADER_LEN..]])
                    } else {
                        bridge.try_fast_path_intercept_batch(&[frame])
                    }
                    .unwrap();
                    assert_eq!(consumed, 1);
                    vec![reply]
                }
            };
            if !matches!(action, Action::PeerReset) {
                assert_eq!(frames.len(), 1);
                let tcp = super::super::packet::tcp_at(&frames[0], ETH_HEADER_LEN).unwrap();
                assert_eq!(tcp.flags() & 0x14, 0x14);
                assert_eq!(tcp.sequence(), 1000);
                assert_eq!(tcp.acknowledgment(), 2000);
                assert_eq!(tcp.ip().source(), dst_ip);
                assert_eq!(tcp.ip().destination(), src_ip);
            }
            assert_eq!(bridge.fast_path_count(), 0);
            assert!(bridge.poll_fast_path().is_empty());
            assert!(released.upgrade().is_none());
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut received))
                .await
                .unwrap()
                .unwrap();
            assert!(received.is_empty());
        }
    }
}
