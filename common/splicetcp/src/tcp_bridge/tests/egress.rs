use super::*;
use crate::egress::{EgressOutcome, EgressResolver, FlowMeta};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, oneshot};

struct Resolver(Mutex<Option<oneshot::Receiver<EgressOutcome>>>);

impl EgressResolver for Resolver {
    fn resolve(&self, _: FlowMeta) -> oneshot::Receiver<EgressOutcome> {
        self.0.lock().unwrap().take().unwrap()
    }
}

fn register(bridge: &mut TcpBridge, port: u16) -> oneshot::Sender<EgressOutcome> {
    register_with_options(bridge, port, &[])
}

pub(super) fn register_with_options(
    bridge: &mut TcpBridge,
    port: u16,
    options: &[u8],
) -> oneshot::Sender<EgressOutcome> {
    let syn = make_guest_syn_frame(port, Ipv4Addr::new(203, 0, 113, 1), 443, 100, options);
    register_syn(bridge, &syn)
}

pub(super) fn register_syn(bridge: &mut TcpBridge, syn: &[u8]) -> oneshot::Sender<EgressOutcome> {
    let (tx, rx) = oneshot::channel();
    bridge.set_egress_resolver(Arc::new(Resolver(Mutex::new(Some(rx)))));
    assert!(bridge.handle_outbound_syn(syn, GW_MAC, GUEST_MAC).is_none());
    assert_eq!(bridge.handshake_count(), 1);
    tx
}

fn expire(bridge: &mut TcpBridge) {
    for conn in bridge.handshake_conns.values_mut() {
        conn.created -= HANDSHAKE_TOTAL_TTL + std::time::Duration::from_millis(1);
    }
}

#[test]
fn drop_resolution_releases_handshake_capacity_without_frames() {
    let mut bridge = TcpBridge::new(GW_IP);
    for _ in 0..=MAX_PENDING_SYNS {
        let tx = register(&mut bridge, 40001);
        assert!(tx.send(EgressOutcome::Drop).is_ok());
        assert!(bridge.poll_handshakes().is_empty());
        assert_eq!(bridge.handshake_count(), 0);
        assert_eq!(bridge.fast_path_count(), 0);
    }
}

#[test]
fn ready_drop_remains_silent_after_handshake_deadline() {
    let mut bridge = TcpBridge::new(GW_IP);
    let tx = register(&mut bridge, 40001);
    assert!(tx.send(EgressOutcome::Drop).is_ok());
    expire(&mut bridge);

    assert!(bridge.poll_handshakes().is_empty());
    assert_eq!(bridge.handshake_count(), 0);
}

#[test]
fn rejected_and_closed_resolutions_reset_the_guest() {
    let mut bridge = TcpBridge::new(GW_IP);
    for outcome in [Some(EgressOutcome::Reject), None] {
        let tx = register(&mut bridge, 40001);
        if let Some(outcome) = outcome {
            assert!(tx.send(outcome).is_ok());
        } else {
            drop(tx);
        }

        let frames = bridge.poll_handshakes();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0][ETH_HEADER_LEN + 20 + 13], 0x14);
        assert_eq!(bridge.handshake_count(), 0);
    }
}

#[tokio::test]
async fn drop_resolution_wakes_the_bridge() {
    let mut bridge = TcpBridge::new(GW_IP);
    let wake = Arc::new(Notify::new());
    bridge.set_handshake_waker(wake.clone());
    let tx = register(&mut bridge, 40001);
    assert!(tx.send(EgressOutcome::Drop).is_ok());

    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .unwrap();
    assert!(bridge.poll_handshakes().is_empty());
    assert_eq!(bridge.handshake_count(), 0);
}

#[tokio::test]
async fn expired_handshake_cancels_the_waker_receiver() {
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_handshake_waker(Arc::new(Notify::new()));
    let mut tx = register(&mut bridge, 40001);
    expire(&mut bridge);
    assert_eq!(bridge.poll_handshakes().len(), 1);
    assert_eq!(bridge.handshake_count(), 0);

    tokio::time::timeout(std::time::Duration::from_secs(1), tx.closed())
        .await
        .unwrap();
}
