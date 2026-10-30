//! Exercises cancellation and connection reuse for Kubernetes listing on the blocking transport.

use std::io::{Read as _, Write as _};
use std::os::fd::IntoRawFd as _;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use arcbox_connect::v1::AgentPingResponse;

use super::*;

fn receive(guest: &mut UnixStream, expected: MessageType) {
    let mut length = [0; 4];
    guest.read_exact(&mut length).unwrap();
    let mut frame = vec![0; u32::from_be_bytes(length) as usize];
    guest.read_exact(&mut frame).unwrap();
    assert_eq!(
        u32::from_be_bytes(frame[..4].try_into().unwrap()),
        expected as u32
    );
}

fn admit(guest: &mut UnixStream) {
    guest
        .write_all(&wire::build_message(
            MessageType::PingResponse,
            "",
            &AgentPingResponse {
                protocol_version: 7,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
        .unwrap();
}

#[tokio::test]
async fn cancellation_closes_handshake_and_listing_reads() {
    for admitted in [false, true] {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let peer = std::thread::spawn(move || {
            receive(&mut guest, MessageType::PingRequest);
            if admitted {
                admit(&mut guest);
                receive(&mut guest, MessageType::KubernetesLoadBalancersRequest);
            }
            started.send(()).unwrap();
            assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
        });
        let client = AgentClient::from_fd_blocking(3, host.into_raw_fd()).unwrap();
        let cancelled = CancellationToken::new();
        let operation_cancelled = cancelled.clone();
        let operation = tokio::spawn(async move {
            client
                .list_kubernetes_load_balancers(&operation_cancelled)
                .await
        });
        ready.await.unwrap();
        cancelled.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), operation)
            .await
            .unwrap()
            .unwrap()
            .err()
            .expect("cancelled listing must fail");
        assert!(error.to_string().contains("cancelled"));
        peer.join().unwrap();
    }
}

#[tokio::test]
async fn successful_listing_returns_the_admitted_connection_for_reuse() {
    let (host, mut guest) = UnixStream::pair().unwrap();
    guest
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let peer = std::thread::spawn(move || {
        receive(&mut guest, MessageType::PingRequest);
        admit(&mut guest);
        for running in [false, true] {
            receive(&mut guest, MessageType::KubernetesLoadBalancersRequest);
            guest
                .write_all(&wire::build_message(
                    MessageType::KubernetesLoadBalancersResponse,
                    "",
                    &KubernetesLoadBalancersResponse {
                        running,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
                .unwrap();
        }
        assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
    });
    let mut client = AgentClient::from_fd_blocking(3, host.into_raw_fd()).unwrap();
    let cancelled = CancellationToken::new();
    for running in [false, true] {
        let (returned, response) = client
            .list_kubernetes_load_balancers(&cancelled)
            .await
            .unwrap();
        assert_eq!(response.running, running);
        client = returned;
    }
    drop(client);
    peer.join().unwrap();
}
