//! Regression coverage for UDP association identity and listener ownership.
use super::*;
use std::time::Duration;

const GW_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GW_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
const GUEST_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];

fn peer(port: u16) -> InboundUdpPeer {
    InboundUdpPeer {
        listener: "127.0.0.1:7177".parse().unwrap(),
        client: SocketAddr::from(([127, 0, 0, 1], port)),
    }
}

fn source_port(frame: &[u8]) -> u16 {
    let offset = ETH_HEADER_LEN + 20;
    u16::from_be_bytes([frame[offset], frame[offset + 1]])
}

fn reply(port: u16, payload: &[u8]) -> Vec<u8> {
    build_udp_ip_ethernet(
        GUEST_IP, GW_IP, 7177, port, payload, GUEST_MAC, GW_MAC, 1500,
    )
    .remove(0)
}

#[tokio::test]
async fn association_preserves_source_port_and_routes_each_clients_replies() {
    let (tx, mut frames) = mpsc::channel(16);
    let mut relay = InboundRelay::new(tx, GW_MAC, GW_IP, GUEST_IP, 1500);
    let (client_tx, mut responses) = mpsc::channel(16);
    let first = peer(45001);
    let second = peer(45002);
    let mut ports = Vec::new();
    for (client, data) in [
        (first, b"initial".as_slice()),
        (second, b"other"),
        (first, b""),
    ] {
        relay.inject_udp(client, data, client_tx.clone(), GUEST_MAC);
        let frame = frames.recv().await.unwrap();
        assert_eq!(&frame[ETH_HEADER_LEN + 28..], data);
        ports.push(source_port(&frame));
    }
    assert_eq!(ports[0], ports[2]);
    assert_ne!(ports[0], ports[1]);
    assert_eq!(relay.udp_flows.len(), 2);
    assert_eq!(relay.udp_clients.len(), 2);

    assert!(relay.try_handle_reply(&reply(ports[1], b"second"), GUEST_MAC));
    assert!(relay.try_handle_reply(&reply(ports[0], b""), GUEST_MAC));
    assert_eq!(
        responses.recv().await.unwrap(),
        (second.client, b"second".to_vec())
    );
    assert_eq!(responses.recv().await.unwrap(), (first.client, Vec::new()));
}

#[tokio::test]
async fn expired_and_recreated_listener_associations_cannot_receive_old_replies() {
    let (tx, mut frames) = mpsc::channel(16);
    let mut relay = InboundRelay::new(tx, GW_MAC, GW_IP, GUEST_IP, 1500);
    let (client_tx, responses) = mpsc::channel(16);
    let client = peer(45001);
    relay.inject_udp(client, b"first", client_tx.clone(), GUEST_MAC);
    let old_port = source_port(&frames.recv().await.unwrap());
    relay.udp_flows.values_mut().next().unwrap().last_active =
        Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
    relay.cleanup();
    assert!(relay.udp_flows.is_empty());
    assert!(relay.udp_clients.is_empty());
    relay.inject_udp(client, b"new", client_tx.clone(), GUEST_MAC);
    let fresh_port = source_port(&frames.recv().await.unwrap());
    assert_ne!(old_port, fresh_port);
    assert!(!relay.try_handle_reply(&reply(old_port, b"old"), GUEST_MAC));

    let (new_tx, new_rx) = mpsc::channel(16);
    relay.inject_udp(client, b"rebound", new_tx, GUEST_MAC);
    let rebound_port = source_port(&frames.recv().await.unwrap());
    assert_ne!(fresh_port, rebound_port);
    assert!(!relay.try_handle_reply(&reply(fresh_port, b"stale"), GUEST_MAC));
    assert_eq!(relay.udp_flows.len(), 1);
    assert_eq!(relay.udp_clients.len(), 1);

    drop(responses);
    relay.inject_udp(peer(45002), b"cancelled", client_tx, GUEST_MAC);
    assert!(frames.try_recv().is_err());
    drop(new_rx);
    relay.cleanup();
    assert!(relay.udp_flows.is_empty());
    assert!(relay.udp_clients.is_empty());
}

#[tokio::test]
async fn port_wrap_and_exhaustion_never_replace_live_associations() {
    let (tx, mut frames) = mpsc::channel(16);
    let mut relay = InboundRelay::new(tx, GW_MAC, GW_IP, GUEST_IP, 1500);
    let (client_tx, _responses) = mpsc::channel(16);
    relay.inject_udp(peer(45001), b"first", client_tx.clone(), GUEST_MAC);
    let first = source_port(&frames.recv().await.unwrap());
    relay.ephemeral_ports.next = first;
    relay.inject_udp(peer(45002), b"second", client_tx.clone(), GUEST_MAC);
    let second = source_port(&frames.recv().await.unwrap());
    assert_ne!(first, second);
    assert_eq!(
        relay.udp_flows[&(GW_IP, first, GUEST_IP, 7177)].peer,
        peer(45001)
    );

    relay.udp_clients.clear();
    relay.udp_flows.clear();
    for port in EPHEMERAL_START..=EPHEMERAL_END {
        let client = peer(port);
        relay.udp_clients.insert(client, port);
        relay.udp_flows.insert(
            (GW_IP, port, GUEST_IP, 7177),
            InboundUdpFlow {
                peer: client,
                client_tx: client_tx.clone(),
                last_active: Instant::now(),
            },
        );
    }
    let count = relay.udp_flows.len();
    relay.inject_udp(peer(45003), b"overflow", client_tx, GUEST_MAC);
    assert!(frames.try_recv().is_err());
    assert_eq!(relay.udp_flows.len(), count);
    assert!(!relay.udp_clients.contains_key(&peer(45003)));
}

#[tokio::test]
async fn listener_removal_closes_reply_channel_and_releases_udp_socket() {
    let (cmd_tx, mut commands) = mpsc::channel(16);
    let mut manager = InboundListenerManager::new(cmd_tx);
    let port = manager
        .add_rule(Ipv4Addr::LOCALHOST, 0, 7177, InboundProtocol::Udp)
        .await
        .unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"request", (Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
        .await
        .unwrap()
        .unwrap();
    let InboundCommand::UdpReceived {
        host_addr,
        client_addr,
        reply_tx,
        data,
        ..
    } = command
    else {
        panic!("UDP listener returned a TCP command");
    };
    assert_eq!(host_addr, SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    assert_eq!(client_addr, client.local_addr().unwrap());
    assert_eq!(data, b"request");
    reply_tx
        .send((client_addr, b"response".to_vec()))
        .await
        .unwrap();
    let mut response = [0; 16];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&response[..length], b"response");
    manager
        .remove_rule(Ipv4Addr::LOCALHOST, 0, InboundProtocol::Udp)
        .await;
    assert!(reply_tx.is_closed());
    UdpSocket::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("listener must release its socket before removal completes");
}
