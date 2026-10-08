//! Exercises delivery, source invalidation, and joined cancellation without a native VM or Keychain.

use super::*;
use arcbox_connect::v1::{
    AgentPingRequest, AgentPingResponse, ReconcileGuestTrustRequest, ReconcileGuestTrustResponse,
};
use arcbox_constants::wire::{AGENT_PROTOCOL_VERSION, MessageType};
use arcbox_transport::vsock::read_framed;
use buffa::Message;
use std::{
    os::fd::IntoRawFd,
    pin::pin,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream,
    sync::oneshot,
};

const PEM: &[u8] = b"public certificate transport fixture";

fn connection(blocking: bool) -> (AgentClient, UnixStream) {
    let (host, guest) = std::os::unix::net::UnixStream::pair().unwrap();
    let client = if blocking {
        AgentClient::from_fd_blocking(3, host.into_raw_fd())
    } else {
        AgentClient::from_fd_async(3, host.into_raw_fd())
    }
    .unwrap();
    guest.set_nonblocking(true).unwrap();
    (client, UnixStream::from_std(guest).unwrap())
}

async fn receive<T: Message>(peer: &mut UnixStream, kind: MessageType) -> T {
    let frame = read_framed(peer).await.unwrap();
    assert_eq!(&frame[4..8], &(kind as u32).to_be_bytes());
    let trace_length = u16::from_be_bytes(frame[8..10].try_into().unwrap()) as usize;
    T::decode_from_slice(&frame[10 + trace_length..]).unwrap()
}

async fn respond(peer: &mut UnixStream, kind: MessageType, message: &impl Message) {
    peer.write_all(&AgentClient::build_message(
        kind,
        "",
        &message.encode_to_vec(),
    ))
    .await
    .unwrap();
}

async fn admit(peer: &mut UnixStream) {
    let _: AgentPingRequest = receive(peer, MessageType::PingRequest).await;
    respond(
        peer,
        MessageType::PingResponse,
        &AgentPingResponse {
            protocol_version: AGENT_PROTOCOL_VERSION,
            ..Default::default()
        },
    )
    .await;
    let request: ReconcileGuestTrustRequest =
        receive(peer, MessageType::ReconcileGuestTrustRequest).await;
    assert_eq!(request.desired, Some(Desired::CertificatePem(PEM.to_vec())));
}

#[tokio::test]
async fn source_changes_ignore_republication_and_observe_missing_ca_or_new_vm() {
    for change in 0..3 {
        let (authority_tx, authority) = watch::channel(Some(1));
        let (vm_tx, vm) = watch::channel(1);
        let current = || Ok(*authority.borrow() == Some(1) && *vm.borrow() == 1);
        let mut changed = pin!(source_changed(authority.clone(), vm.clone(), &current));
        assert!(matches!(futures::poll!(&mut changed), Poll::Pending));
        authority_tx.send_replace(Some(1));
        assert!(matches!(futures::poll!(&mut changed), Poll::Pending));
        match change {
            0 => {
                authority_tx.send_replace(None);
            }
            1 => {
                authority_tx.send_replace(Some(2));
            }
            _ => {
                vm_tx.send_replace(2);
            }
        }
        assert!(matches!(futures::poll!(&mut changed), Poll::Ready(Ok(()))));
    }
}

#[tokio::test]
async fn an_unpolled_cancelled_or_obsolete_attempt_never_connects() {
    for cancel in [false, true] {
        let shutdown = CancellationToken::new();
        let active = AtomicBool::new(true);
        let current = || Ok(active.load(Ordering::SeqCst));
        let delivery = attempt(
            async { panic!("an obsolete attempt must not connect") },
            PEM,
            "root",
            &shutdown,
            &current,
            std::future::pending(),
        );
        if cancel {
            shutdown.cancel();
        } else {
            active.store(false, Ordering::SeqCst);
        }
        assert!(!delivery.await.unwrap());
    }
}

#[tokio::test]
async fn native_connection_finishes_before_cancelled_or_obsolete_delivery_returns() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for cancel in [false, true] {
            let (client, mut peer) = connection(true);
            let shutdown = CancellationToken::new();
            let active = AtomicBool::new(true);
            let current = || Ok(active.load(Ordering::SeqCst));
            let (connected, connection) = oneshot::channel();
            let mut delivery = pin!(attempt(
                async { Ok(connection.await?) },
                PEM,
                "root",
                &shutdown,
                &current,
                std::future::pending(),
            ));
            assert!(matches!(futures::poll!(&mut delivery), Poll::Pending));
            if cancel {
                shutdown.cancel();
            } else {
                active.store(false, Ordering::SeqCst);
            }
            assert!(matches!(futures::poll!(&mut delivery), Poll::Pending));
            assert!(connected.send(client).is_ok());
            assert!(!delivery.await.unwrap());
            assert_eq!(peer.read(&mut [0; 1]).await.unwrap(), 0);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn acknowledgement_requires_the_requested_fingerprint_and_current_source() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for blocking in [false, true] {
            for (fingerprint, remains_current) in [("root", true), ("other", true), ("root", false)]
            {
                let (client, mut peer) = connection(blocking);
                let shutdown = CancellationToken::new();
                let active = AtomicBool::new(true);
                let current = || Ok(active.load(Ordering::SeqCst));
                let delivery = attempt(
                    async { Ok(client) },
                    PEM,
                    "root",
                    &shutdown,
                    &current,
                    std::future::pending(),
                );
                let guest = async {
                    admit(&mut peer).await;
                    active.store(remains_current, Ordering::SeqCst);
                    respond(
                        &mut peer,
                        MessageType::ReconcileGuestTrustResponse,
                        &ReconcileGuestTrustResponse {
                            applied: Some(Applied::InstalledSha256(fingerprint.into())),
                            ..Default::default()
                        },
                    )
                    .await;
                };
                let (result, ()) = tokio::join!(delivery, guest);
                if fingerprint == "root" {
                    assert_eq!(result.unwrap(), remains_current);
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("different CA fingerprint")
                    );
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn source_invalidation_and_shutdown_close_pending_rpc_before_returning() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for blocking in [false, true] {
            for cancel in [false, true] {
                let (client, mut peer) = connection(blocking);
                let shutdown = CancellationToken::new();
                let (invalidate, changed) = oneshot::channel();
                let mut invalidate = Some(invalidate);
                let current = || Ok(true);
                let delivery = attempt(
                    async { Ok(client) },
                    PEM,
                    "root",
                    &shutdown,
                    &current,
                    async { Ok(changed.await?) },
                );
                let guest = async {
                    admit(&mut peer).await;
                    if cancel {
                        shutdown.cancel();
                    } else {
                        invalidate.take().unwrap().send(()).unwrap();
                    }
                    assert_eq!(peer.read(&mut [0; 1]).await.unwrap(), 0);
                };
                let (result, ()) = tokio::join!(delivery, guest);
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("cancelled"),
                    "blocking={blocking}, shutdown={cancel}: {error:#}"
                );
            }
        }
    })
    .await
    .unwrap();
}
