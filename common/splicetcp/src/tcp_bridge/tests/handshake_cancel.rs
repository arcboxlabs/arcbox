//! Cancellation at queued dial, pending SYN-ACK, and final ACK boundaries.

use super::*;
use crate::{EgressConn, EgressOutcome, EgressResolver, FlowMeta, NativeTcp};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct PendingResolver(Mutex<Option<oneshot::Receiver<EgressOutcome>>>);

impl EgressResolver for PendingResolver {
    fn resolve(&self, _flow: FlowMeta) -> oneshot::Receiver<EgressOutcome> {
        self.0.lock().unwrap().take().unwrap()
    }
}

fn pending(wake: bool) -> (TcpBridge, oneshot::Sender<EgressOutcome>) {
    let (tx, rx) = oneshot::channel();
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.set_egress_resolver(Arc::new(PendingResolver(Mutex::new(Some(rx)))));
    if wake {
        bridge.set_handshake_waker(Arc::new(tokio::sync::Notify::new()));
    }
    let syn = make_guest_syn_frame(40400, Ipv4Addr::new(203, 0, 113, 1), 443, 2000, &[]);
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    (bridge, tx)
}

async fn managed_pair() -> (
    NativeTcp<tokio::net::TcpStream>,
    tokio::net::TcpStream,
    CancellationToken,
    std::sync::Weak<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept(),
    );
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let cancel = CancellationToken::new();
    (
        NativeTcp::with_lifecycle(client.unwrap(), lease, cancel.clone()),
        accepted.unwrap().0,
        cancel,
        released,
    )
}

#[derive(Clone, Copy)]
enum Stage {
    Queued,
    SynAck,
    FinalAck,
    PeerReset,
}

#[tokio::test]
async fn cancelled_passive_handshakes_release_and_preserve_explicit_abandonment() {
    for (stage, abandon) in [
        Stage::Queued,
        Stage::SynAck,
        Stage::FinalAck,
        Stage::PeerReset,
    ]
    .into_iter()
    .flat_map(|stage| [false, true].map(|abandon| (stage, abandon)))
    {
        let (stream, _peer, cancel, released) = managed_pair().await;
        let (mut bridge, tx) = pending(false);
        let (transport, handle) = if abandon {
            let (transport, handle) = EgressConn::controlled_tcp(stream);
            (transport, Some(handle))
        } else {
            (EgressConn::Tcp(stream), None)
        };
        assert!(tx.send(EgressOutcome::Connect(transport)).is_ok());
        let syn_ack = if matches!(stage, Stage::Queued) {
            None
        } else {
            Some(bridge.poll_handshakes().pop().unwrap())
        };
        if let Some(handle) = handle {
            handle.abandon();
        }
        cancel.cancel();
        let reset_seq = syn_ack.as_ref().map_or(0, |frame| {
            super::super::packet::tcp_at(frame, ETH_HEADER_LEN)
                .unwrap()
                .sequence()
                .wrapping_add(1)
        });
        let replies = match stage {
            Stage::Queued | Stage::SynAck => bridge.poll_handshakes(),
            Stage::FinalAck | Stage::PeerReset => {
                let syn_ack = syn_ack.unwrap();
                let sent = super::super::packet::tcp_at(&syn_ack, ETH_HEADER_LEN).unwrap();
                let frame = make_guest_segment(
                    (40400, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), 443),
                    2001,
                    sent.sequence().wrapping_add(1),
                    65535,
                    if matches!(stage, Stage::PeerReset) {
                        0x14
                    } else {
                        0x18
                    },
                    b"must not be uploaded",
                );
                bridge.try_complete_handshake(&frame).unwrap()
            }
        };
        if abandon || matches!(stage, Stage::PeerReset) {
            assert!(replies.is_empty());
        } else {
            assert_eq!(replies.len(), 1);
            let rst = super::super::packet::tcp_at(&replies[0], ETH_HEADER_LEN).unwrap();
            assert_eq!(rst.flags(), 0x14);
            assert_eq!(rst.sequence(), reset_seq);
            assert_eq!(rst.acknowledgment(), 2001);
        }
        assert_eq!(bridge.handshake_count(), 0);
        assert_eq!(bridge.fast_path_count(), 0);
        assert!(released.upgrade().is_none());
    }
}

#[tokio::test]
async fn cancelled_active_handshake_resets_only_a_sent_syn() {
    for (isn, sent) in [(42_u32, false), (42, true), (u32::MAX, true)] {
        let (stream, _peer, cancel, released) = managed_pair().await;
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 40400,
            dst_ip: GW_IP.into(),
            dst_port: 443,
        };
        let mut bridge = TcpBridge::new(GW_IP);
        bridge.initiate_active_handshake(key, stream.into_std().unwrap(), GW_MAC, GUEST_MAC);
        bridge.handshake_conns.get_mut(&key).unwrap().our_isn = isn;
        if sent {
            assert_eq!(bridge.poll_handshakes().len(), 1);
        }
        cancel.cancel();
        let replies = bridge.poll_handshakes();
        if sent {
            assert_eq!(replies.len(), 1);
            let rst = super::super::packet::tcp_at(&replies[0], ETH_HEADER_LEN).unwrap();
            assert_eq!(rst.flags(), 0x04);
            assert_eq!(rst.sequence(), isn.wrapping_add(1));
        } else {
            assert!(replies.is_empty());
        }
        assert_eq!(bridge.handshake_count(), 0);
        assert!(released.upgrade().is_none());
    }
}

#[tokio::test]
async fn bridge_drop_releases_the_waker_forwarding_receiver() {
    let (bridge, mut sender) = pending(true);
    drop(bridge);
    tokio::time::timeout(Duration::from_secs(2), sender.closed())
        .await
        .unwrap();
}
