//! Managed socket ownership through bridge queues, promotion, and teardown.

use super::*;
use crate::direct_rx::{ConnSink, PromotedConn};
use crate::{EgressConn, EgressResolver, FlowMeta, NativeTcp};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(2);

fn key() -> SynFlowKey {
    SynFlowKey {
        src_ip: GUEST_IP.into(),
        src_port: 40400,
        dst_ip: Ipv4Addr::new(198, 18, 0, 1).into(),
        dst_port: 443,
    }
}

fn bridge() -> TcpBridge {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge
}

async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    tokio::time::timeout(TIMEOUT, async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept(),
        );
        (client.unwrap(), accepted.unwrap().0)
    })
    .await
    .expect("the loopback socket pair must connect")
}

#[tokio::test]
async fn cancelled_native_flows_reset_on_poll_and_intercept() {
    for intercept in [false, true] {
        let (client, mut peer) = socket_pair().await;
        let lease = Arc::new(());
        let released = Arc::downgrade(&lease);
        let cancelled = CancellationToken::new();
        let stream =
            NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, cancelled.clone());
        let mut bridge = bridge();
        bridge.promote_to_fast_path(key(), stream, 1000, 2000, 1460, None);
        cancelled.cancel();
        assert!(
            released.upgrade().is_some(),
            "the unpolled primary owns its socket"
        );
        let frames = if intercept {
            let upload = make_guest_segment(
                (key().src_port, key().dst_ip, key().dst_port),
                2000,
                1000,
                65535,
                0x18,
                b"must not upload after cancellation",
            );
            vec![bridge.try_fast_path_intercept(&upload).unwrap()]
        } else {
            bridge.poll_fast_path()
        };
        assert_eq!(frames.len(), 1);
        assert_ne!(frames[0][ETH_HEADER_LEN + 20 + 13] & 0x04, 0);
        assert_eq!(bridge.fast_path_count(), 0);
        assert!(released.upgrade().is_none());
        let mut received = Vec::new();
        tokio::time::timeout(TIMEOUT, peer.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert!(received.is_empty());
    }
}

struct UnmanagedSink;

impl ConnSink for UnmanagedSink {
    fn send_conn(&self, conn: PromotedConn) -> bool {
        assert!(matches!(
            conn.stream.into_unmanaged(),
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported
        ));
        false
    }
}

#[tokio::test]
async fn unmanaged_sink_rejection_keeps_the_managed_primary_usable() {
    let (client, mut peer) = socket_pair().await;
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let cancelled = CancellationToken::new();
    let stream = NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, cancelled.clone());
    let mut bridge = bridge();
    bridge.set_conn_sink(Arc::new(UnmanagedSink));
    bridge.promote_to_fast_path(key(), stream, 1000, 2000, 1460, None);
    assert!(!bridge.fast_path_conns[&key()].inline_owned);
    assert!(!cancelled.is_cancelled());
    let upload = make_guest_segment(
        (key().src_port, key().dst_ip, key().dst_port),
        2000,
        1000,
        65535,
        0x18,
        b"upload",
    );
    bridge.try_fast_path_intercept(&upload).unwrap();
    let mut received = [0; 6];
    tokio::time::timeout(TIMEOUT, peer.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received, b"upload");
    drop(bridge);
    assert!(released.upgrade().is_none());
}

#[derive(Default)]
struct HoldingSink(Mutex<Vec<PromotedConn>>);

impl ConnSink for HoldingSink {
    fn send_conn(&self, conn: PromotedConn) -> bool {
        self.0.lock().unwrap().push(conn);
        true
    }
}

#[tokio::test]
async fn replacement_and_bridge_drop_cancel_only_their_reader_incarnation() {
    let (first, _first_peer) = socket_pair().await;
    let (second, _second_peer) = socket_pair().await;
    let cancelled = CancellationToken::new();
    let first_lease = Arc::new(());
    let first_released = Arc::downgrade(&first_lease);
    let second_lease = Arc::new(());
    let second_released = Arc::downgrade(&second_lease);
    let sink = Arc::new(HoldingSink::default());
    let mut bridge = bridge();
    bridge.set_conn_sink(sink.clone());
    bridge.promote_to_fast_path(
        key(),
        NativeTcp::with_lifecycle(first.into_std().unwrap(), first_lease, cancelled.clone()),
        1000,
        2000,
        1460,
        None,
    );
    let first_reader = sink.0.lock().unwrap().pop().unwrap();
    assert!(!first_reader.reader_cancel.is_cancelled());
    bridge.promote_to_fast_path(
        key(),
        NativeTcp::with_lifecycle(second.into_std().unwrap(), second_lease, cancelled.clone()),
        3000,
        4000,
        1460,
        None,
    );
    let second_reader = sink.0.lock().unwrap().pop().unwrap();
    assert!(first_reader.reader_cancel.is_cancelled());
    assert!(!second_reader.reader_cancel.is_cancelled());
    assert!(!cancelled.is_cancelled());
    assert!(
        first_released.upgrade().is_some(),
        "the held reader still owns a descriptor"
    );
    drop(first_reader);
    assert!(first_released.upgrade().is_none());
    drop(bridge);
    assert!(second_reader.reader_cancel.is_cancelled());
    assert!(!cancelled.is_cancelled());
    assert!(second_released.upgrade().is_some());
    drop(second_reader);
    assert!(second_released.upgrade().is_none());
}

struct PendingResolver(Mutex<Option<oneshot::Receiver<Option<EgressConn>>>>);

impl EgressResolver for PendingResolver {
    fn resolve(&self, _flow: FlowMeta) -> oneshot::Receiver<Option<EgressConn>> {
        self.0.lock().unwrap().take().unwrap()
    }
}

fn pending_handshake(receiver: oneshot::Receiver<Option<EgressConn>>, wake: bool) -> TcpBridge {
    let mut bridge = bridge();
    bridge.set_egress_resolver(Arc::new(PendingResolver(Mutex::new(Some(receiver)))));
    if wake {
        bridge.set_handshake_waker(Arc::new(tokio::sync::Notify::new()));
    }
    let syn = make_guest_syn_frame(key().src_port, key().dst_ip, key().dst_port, 2000, &[]);
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    bridge
}

#[tokio::test]
async fn cancelled_queued_dial_is_dropped_when_the_handshake_is_polled() {
    let (client, _peer) = socket_pair().await;
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let cancelled = CancellationToken::new();
    let (sender, receiver) = oneshot::channel();
    let mut bridge = pending_handshake(receiver, false);
    assert!(
        sender
            .send(Some(EgressConn::Tcp(NativeTcp::with_lifecycle(
                client,
                lease,
                cancelled.clone(),
            ))))
            .is_ok()
    );
    cancelled.cancel();
    assert!(released.upgrade().is_some());
    let replies = bridge.poll_handshakes();
    assert_eq!(replies.len(), 1);
    assert_ne!(replies[0][ETH_HEADER_LEN + 20 + 13] & 0x04, 0);
    assert_eq!(bridge.handshake_count(), 0);
    assert!(released.upgrade().is_none());
}

#[tokio::test]
async fn dropping_the_handshake_cancels_the_forwarding_receiver() {
    let (mut sender, receiver) = oneshot::channel();
    let bridge = pending_handshake(receiver, true);
    drop(bridge);
    tokio::time::timeout(TIMEOUT, sender.closed())
        .await
        .expect("the forwarding task must release the resolver receiver");
}

mod abort;
