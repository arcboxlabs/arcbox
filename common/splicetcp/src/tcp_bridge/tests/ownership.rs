//! Socket-owner leases survive handshake, descriptor cloning, and reader handoff.

use super::*;
use crate::direct_rx::{ConnSink, PromotedConn};
use crate::{EgressConn, EgressResolver, FlowMeta, NativeTcp};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    tokio::time::timeout(TIMEOUT, async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept(),
        );
        (client.unwrap(), accepted.unwrap().0)
    })
    .await
    .unwrap()
}

struct Resolver(Mutex<Option<EgressConn>>);

impl EgressResolver for Resolver {
    fn resolve(&self, _flow: FlowMeta) -> oneshot::Receiver<Option<EgressConn>> {
        let (tx, rx) = oneshot::channel();
        assert!(tx.send(self.0.lock().unwrap().take()).is_ok());
        rx
    }
}

#[derive(Default)]
struct HoldingSink(Mutex<Option<PromotedConn>>);

impl ConnSink for HoldingSink {
    fn send_conn(&self, conn: PromotedConn) -> bool {
        *self.0.lock().unwrap() = Some(conn);
        true
    }
}

#[tokio::test]
async fn passive_handshake_and_reader_handoff_retain_the_socket_owner() {
    let (client, mut peer) = socket_pair().await;
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let stream = NativeTcp::with_lifecycle(client, lease, CancellationToken::new());
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_egress_resolver(Arc::new(Resolver(Mutex::new(Some(EgressConn::Tcp(
        stream,
    ))))));
    let sink = Arc::new(HoldingSink::default());
    bridge.set_conn_sink(sink.clone());
    let dst = Ipv4Addr::new(203, 0, 113, 1);
    let syn = make_guest_syn_frame(40400, dst, 443, 2000, &[2, 4, 0x05, 0xb4]);
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    let replies = bridge.poll_handshakes();
    assert_eq!(replies.len(), 1);
    let seq = super::super::packet::tcp_at(&replies[0], ETH_HEADER_LEN)
        .unwrap()
        .sequence()
        .wrapping_add(1);
    assert!(released.upgrade().is_some());
    bridge
        .try_complete_handshake(&make_guest_ack_frame(40400, dst, 443, 2001, seq))
        .unwrap();
    assert_eq!(bridge.handshake_count(), 0);
    let promoted = sink.0.lock().unwrap().take().unwrap();
    drop(bridge);
    assert!(
        released.upgrade().is_some(),
        "the reader still owns a cloned descriptor"
    );
    drop(promoted);
    assert!(released.upgrade().is_none());
    assert_eq!(
        tokio::time::timeout(TIMEOUT, peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

struct UnmanagedSink;

impl ConnSink for UnmanagedSink {
    fn send_conn(&self, conn: PromotedConn) -> bool {
        assert!(matches!(conn.stream.into_unmanaged(), Err(error)
            if error.kind() == std::io::ErrorKind::Unsupported));
        false
    }
}

#[tokio::test]
async fn rejected_reader_handoff_preserves_managed_uploads() {
    let (client, mut peer) = socket_pair().await;
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let stream =
        NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, CancellationToken::new());
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_conn_sink(Arc::new(UnmanagedSink));
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40400,
        dst_ip: Ipv4Addr::new(203, 0, 113, 1).into(),
        dst_port: 443,
    };
    bridge.promote_to_fast_path(key, stream, 1000, 2000, 1460, None);
    assert!(!bridge.fast_path_conns[&key].inline_owned);
    let upload = make_guest_segment(
        (key.src_port, key.dst_ip, key.dst_port),
        2000,
        1000,
        65535,
        0x18,
        b"upload",
    );
    bridge.try_fast_path_intercept(&upload).unwrap();
    let mut bytes = [0; 6];
    tokio::time::timeout(TIMEOUT, peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"upload");
    assert!(released.upgrade().is_some());
    drop(bridge);
    assert!(released.upgrade().is_none());
}

#[tokio::test]
async fn inbound_handshake_drop_releases_the_managed_socket() {
    let (client, mut peer) = socket_pair().await;
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let stream = NativeTcp::with_lifecycle(client, lease, CancellationToken::new());
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.initiate_inbound(8080, stream, fixture_v4(GUEST_IP), GW_IP);
    assert_eq!(bridge.handshake_count(), 1);
    assert!(released.upgrade().is_some());
    drop(bridge);
    assert!(released.upgrade().is_none());
    assert_eq!(
        tokio::time::timeout(TIMEOUT, peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn replacement_and_bridge_drop_cancel_only_their_own_reader() {
    let (first, _first_peer) = socket_pair().await;
    let (second, _second_peer) = socket_pair().await;
    let generation = CancellationToken::new();
    let first_lease = Arc::new(());
    let first_released = Arc::downgrade(&first_lease);
    let second_lease = Arc::new(());
    let second_released = Arc::downgrade(&second_lease);
    let sink = Arc::new(HoldingSink::default());
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_conn_sink(sink.clone());
    let key = SynFlowKey {
        src_ip: GUEST_IP,
        src_port: 40400,
        dst_ip: Ipv4Addr::new(203, 0, 113, 1).into(),
        dst_port: 443,
    };
    for (stream, lease) in [(first, first_lease), (second, second_lease)] {
        let previous = sink.0.lock().unwrap().take();
        let stream =
            NativeTcp::with_lifecycle(stream.into_std().unwrap(), lease, generation.clone());
        bridge.promote_to_fast_path(key, stream, 1000, 2000, 1460, None);
        if let Some(previous) = previous {
            assert!(previous.reader_cancel.is_cancelled());
            assert!(first_released.upgrade().is_some());
            drop(previous);
            assert!(first_released.upgrade().is_none());
        }
        assert!(
            !sink
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .reader_cancel
                .is_cancelled()
        );
        assert!(!generation.is_cancelled());
    }
    let reader = sink.0.lock().unwrap().take().unwrap();
    drop(bridge);
    assert!(reader.reader_cancel.is_cancelled());
    assert!(!generation.is_cancelled());
    assert!(second_released.upgrade().is_some());
    drop(reader);
    assert!(second_released.upgrade().is_none());
}
