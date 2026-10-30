//! Verify startup cleanup retains real DNS queries across a cancelled waiter.

use std::{io, net::Ipv4Addr, task::Poll};

use arcbox_api::SetupState;
use arcbox_net::{NetConfig, NetworkManager, dns::DnsConfig};
use tokio::net::UdpSocket;

use super::*;
use crate::dns_service::DnsService;

#[tokio::test]
async fn failed_startup_joins_dns_before_returning() {
    startup_cleanup_waits_for_dns(false).await;
}

#[tokio::test]
async fn interrupted_startup_joins_dns_before_returning() {
    startup_cleanup_waits_for_dns(true).await;
}

async fn startup_cleanup_waits_for_dns(interrupted: bool) {
    let handles = StartupHandles::new(Arc::new(SetupState::new()));
    assert!(handles.early_runtime.get().is_none());
    assert!(handles.container_network_lease.get().is_none());
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let manager = Arc::new(NetworkManager::new(NetConfig {
        dns: DnsConfig::default().with_upstream(vec![upstream.local_addr().unwrap()]),
        ..Default::default()
    }));
    let retained_manager = Arc::downgrade(&manager);
    let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
    let address = (Ipv4Addr::LOCALHOST, service.host_port().unwrap());
    handles
        .dns_service
        .start(service.run(manager, handles.shutdown.clone()))
        .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07pending\x07example\x03com\x00\x00\x01\x00\x01";
    client.send_to(query, address).await.unwrap();
    let mut packet = [0; 512];
    let (length, peer) =
        tokio::time::timeout(Duration::from_secs(3), upstream.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(&packet[..length], query);

    let mut cleanup = Box::pin(async {
        if interrupted {
            interrupt_startup(&handles).await.unwrap();
        } else {
            abort_failed_startup(&handles).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = &mut cleanup => panic!("startup cleanup abandoned the pending DNS query"),
            () = handles.shutdown.cancelled() => {}
        }
    })
    .await
    .unwrap();
    assert!(matches!(futures::poll!(&mut cleanup), Poll::Pending));
    drop(cleanup);

    let survivor = handles.clone();
    drop(handles);
    let mut cleanup = Box::pin(abort_failed_startup(&survivor));
    assert!(matches!(futures::poll!(&mut cleanup), Poll::Pending));
    assert_eq!(
        UdpSocket::bind(address).await.unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );

    let response = arcbox_dns::build_response_a(query, Ipv4Addr::LOCALHOST, 60).unwrap();
    upstream.send_to(&response, peer).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), &mut cleanup)
        .await
        .unwrap();
    let _replacement = UdpSocket::bind(address).await.unwrap();
    assert!(retained_manager.upgrade().is_none());
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
