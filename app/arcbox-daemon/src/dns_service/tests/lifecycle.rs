//! Exercise DNS shutdown with real upstream requests held in blocking I/O.

use super::*;
use std::{pin::pin, task::Poll, time::Duration};

#[tokio::test]
async fn shutdown_joins_pending_upstream_before_releasing_listener() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig {
        dns: arcbox_net::dns::DnsConfig::default()
            .with_upstream(vec![upstream.local_addr().unwrap()]),
        ..Default::default()
    }));
    let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
    let address = (Ipv4Addr::LOCALHOST, service.host_port().unwrap());
    let shutdown = CancellationToken::new();
    let mut running = pin!(service.run(manager, shutdown.clone()));
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = build_dns_query("pending.example.com");
    client.send_to(&query, address).await.unwrap();

    let mut packet = [0; 512];
    let (length, peer) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut running => panic!("DNS stopped before upstream request: {result:?}"),
            received = upstream.recv_from(&mut packet) => received.unwrap(),
        }
    })
    .await
    .unwrap();
    assert_eq!(&packet[..length], query);

    shutdown.cancel();
    assert!(matches!(futures::poll!(&mut running), Poll::Pending));
    assert_eq!(
        UdpSocket::bind(address).await.unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    let response = arcbox_dns::build_response_a(&query, Ipv4Addr::LOCALHOST, 60).unwrap();
    upstream.send_to(&response, peer).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), &mut running)
        .await
        .unwrap()
        .unwrap();

    let _replacement = UdpSocket::bind(address).await.unwrap();
    assert_eq!(
        client
            .into_std()
            .unwrap()
            .recv_from(&mut packet)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn dropped_service_does_not_leave_listener_in_upstream_worker() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig {
        dns: arcbox_net::dns::DnsConfig::default()
            .with_upstream(vec![upstream.local_addr().unwrap()]),
        ..Default::default()
    }));
    let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
    let address = (Ipv4Addr::LOCALHOST, service.host_port().unwrap());
    let mut running = Box::pin(service.run(manager, CancellationToken::new()));
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = build_dns_query("pending.example.com");
    client.send_to(&query, address).await.unwrap();
    let mut packet = [0; 512];
    let (_, peer) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut running => panic!("DNS stopped before upstream request: {result:?}"),
            received = upstream.recv_from(&mut packet) => received.unwrap(),
        }
    })
    .await
    .unwrap();

    drop(running);
    let _replacement = UdpSocket::bind(address).await.unwrap();
    let response = arcbox_dns::build_response_a(&query, Ipv4Addr::LOCALHOST, 60).unwrap();
    upstream.send_to(&response, peer).await.unwrap();
}

#[tokio::test]
async fn upstream_completion_delivers_answers_and_reports_invalid_responses() {
    for valid_response in [true, false] {
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig {
            dns: arcbox_net::dns::DnsConfig::default()
                .with_upstream(vec![upstream.local_addr().unwrap()]),
            ..Default::default()
        }));
        let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
        let address = (Ipv4Addr::LOCALHOST, service.host_port().unwrap());
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(service.run(manager, shutdown.clone()));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let query = build_dns_query("response.example.com");
        client.send_to(&query, address).await.unwrap();
        let mut packet = [0; 512];
        let (_, peer) =
            tokio::time::timeout(Duration::from_secs(3), upstream.recv_from(&mut packet))
                .await
                .unwrap()
                .unwrap();
        let expected = arcbox_dns::build_response_a(&query, Ipv4Addr::LOCALHOST, 60).unwrap();
        let mut response = expected.clone();
        if !valid_response {
            response[0] ^= 0xff;
        }
        upstream.send_to(&response, peer).await.unwrap();
        let (length, source) =
            tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut packet))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(source, address.into());
        if valid_response {
            assert_eq!(&packet[..length], expected);
        } else {
            assert_eq!(&packet[..length], build_servfail(&query).unwrap());
        }

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
