use super::*;
use arcbox_connect::v1::{AgentPingResponse, InstanceAuthorityAck, OpenInstanceAuthorityResponse};
use arcbox_constants::wire::{AGENT_PROTOCOL_VERSION, MessageType};
use arcbox_dns::{claims::Claim, registry::InstanceAuthority};
use arcbox_transport::vsock::read_framed;
use buffa::Message;
use std::{
    os::fd::IntoRawFd,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::{io::AsyncWriteExt as _, net::UnixStream};

async fn receive<T: Message>(peer: &mut UnixStream, expected: MessageType) -> T {
    let frame = read_framed(peer).await.unwrap();
    assert_eq!(&frame[4..8], &(expected as u32).to_be_bytes());
    assert_eq!(&frame[8..10], &[0, 0]);
    T::decode_from_slice(&frame[10..]).unwrap()
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

async fn acknowledge(peer: &mut UnixStream, snapshot: &ReplaceInstanceAuthorityRequest) {
    respond(
        peer,
        MessageType::InstanceAuthorityAck,
        &InstanceAuthorityAck {
            authority_revision: snapshot.authority_revision,
            ..Default::default()
        },
    )
    .await;
}

fn publish(manager: &NetworkManager, mutate: impl FnOnce(&mut arcbox_dns::registry::Registry)) {
    let registry = manager.dns_registry();
    let mut registry = registry.write().unwrap();
    mutate(&mut registry);
    manager.replace_dns_projection(&registry);
}

fn connection(blocking: bool) -> (AgentClient, UnixStream) {
    let (host, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let client = if blocking {
        AgentClient::from_fd_blocking(3, host.into_raw_fd())
    } else {
        AgentClient::from_fd_async(3, host.into_raw_fd())
    }
    .unwrap();
    peer.set_nonblocking(true).unwrap();
    (client, UnixStream::from_std(peer).unwrap())
}

fn transports() -> impl Iterator<Item = bool> {
    [
        true,
        #[cfg(target_os = "macos")]
        false,
    ]
    .into_iter()
}

async fn admit(peer: &mut UnixStream) {
    let _: arcbox_connect::v1::AgentPingRequest = receive(peer, MessageType::PingRequest).await;
    respond(
        peer,
        MessageType::PingResponse,
        &AgentPingResponse {
            protocol_version: AGENT_PROTOCOL_VERSION,
            ..Default::default()
        },
    )
    .await;
    let _: arcbox_connect::v1::OpenInstanceAuthorityRequest =
        receive(peer, MessageType::OpenInstanceAuthorityRequest).await;
}

async fn open(peer: &mut UnixStream) {
    respond(
        peer,
        MessageType::OpenInstanceAuthorityResponse,
        &OpenInstanceAuthorityResponse::default(),
    )
    .await;
}

#[tokio::test]
async fn delivery_sends_complete_authority_and_preserves_source_lifetimes() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, mut peer) = connection(true);
        let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
        let shutdown = CancellationToken::new();
        let (_state, vm) = watch::channel(VmLifecycleState::Stopped);
        let task = tokio::spawn({
            let manager = Arc::clone(&manager);
            let shutdown = shutdown.clone();
            async move { connected(client, &manager, &shutdown, vm, || Ok(())).await }
        });
        admit(&mut peer).await;
        open(&mut peer).await;
        let empty: ReplaceInstanceAuthorityRequest =
            receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
        assert_eq!(empty.roots, []);
        acknowledge(&mut peer, &empty).await;

        let roots: Vec<_> = [
            ("a.local", InstanceTarget::Current),
            ("b.local", InstanceTarget::Other { port: 5554 }),
            ("c.local", InstanceTarget::Unavailable),
        ]
        .into_iter()
        .map(|(name, target)| InstanceAuthority {
            root: name.parse().unwrap(),
            target,
        })
        .collect();
        publish(&manager, |registry| {
            registry.replace_instance_authorities(roots.clone());
        });
        let active: ReplaceInstanceAuthorityRequest =
            receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
        assert!(active.authority_revision > empty.authority_revision);
        assert_eq!(active.roots.len(), 3);
        assert_eq!(active.roots[0].target, Target::Current);
        assert_eq!(active.roots[1].target, Target::Other);
        assert_eq!(active.roots[1].dns_port, 5554);
        assert_eq!(active.roots[2].target, Target::Unavailable);
        acknowledge(&mut peer, &active).await;
        publish(&manager, |registry| {
            registry
                .replace(
                    "container".into(),
                    vec![Claim::exclusive(
                        arcbox_dns::NamePattern::Exact("web.a.local".parse().unwrap()),
                        "container".into(),
                        "containers".into(),
                    )],
                    Some("10.0.2.20".parse().unwrap()),
                )
                .unwrap();
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), read_framed(&mut peer))
                .await
                .is_err()
        );
        publish(&manager, |registry| {
            registry.invalidate_instance_authorities();
            registry.replace_instance_authorities(roots);
        });
        let renewed: ReplaceInstanceAuthorityRequest =
            receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
        assert_eq!(renewed.roots, active.roots);
        assert!(renewed.authority_revision > active.authority_revision);
        acknowledge(&mut peer, &renewed).await;
        publish(&manager, |registry| {
            registry.replace_instance_authorities(Vec::new());
        });
        let removed: ReplaceInstanceAuthorityRequest =
            receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
        assert_eq!(removed.roots, []);
        assert!(removed.authority_revision > renewed.authority_revision);
        acknowledge(&mut peer, &removed).await;
        shutdown.cancel();
        task.await.unwrap().unwrap();
        assert!(read_framed(&mut peer).await.is_err());
    })
    .await
    .expect("authority delivery did not finish");
}

#[tokio::test]
async fn physical_vm_loss_and_shutdown_join_pending_authority_delivery() {
    for blocking in transports() {
        for boundary in ["open", "ack", "lifecycle", "shutdown"] {
            tokio::time::timeout(Duration::from_secs(3), async {
                let (client, mut peer) = connection(blocking);
                let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
                let shutdown = CancellationToken::new();
                let (state, vm) = watch::channel(VmLifecycleState::Running);
                let valid = Arc::new(AtomicBool::new(true));
                let task = tokio::spawn({
                    let manager = Arc::clone(&manager);
                    let shutdown = shutdown.clone();
                    let valid = Arc::clone(&valid);
                    async move {
                        connected(client, &manager, &shutdown, vm, || {
                            ensure!(valid.load(Ordering::Acquire), "VM incarnation changed");
                            Ok(())
                        })
                        .await
                    }
                });
                admit(&mut peer).await;
                if boundary == "open" {
                    valid.store(false, Ordering::Release);
                }
                open(&mut peer).await;
                if boundary != "open" {
                    let pending: ReplaceInstanceAuthorityRequest =
                        receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
                    if boundary == "shutdown" {
                        shutdown.cancel();
                    } else {
                        valid.store(false, Ordering::Release);
                        if boundary == "lifecycle" {
                            state.send(VmLifecycleState::Stopped).unwrap();
                        } else {
                            acknowledge(&mut peer, &pending).await;
                        }
                    }
                }
                let result = task.await.unwrap();
                if boundary == "shutdown" {
                    result.unwrap();
                } else {
                    let error = result.unwrap_err();
                    assert!(
                        error.to_string().contains("VM incarnation changed"),
                        "{error}"
                    );
                }
                assert!(read_framed(&mut peer).await.is_err());
            })
            .await
            .unwrap_or_else(|_| {
                panic!("delivery did not close at {boundary}, blocking={blocking}")
            });
        }
    }
}

#[tokio::test]
async fn idle_eof_joins_before_a_fresh_session_replays_complete_authority() {
    for blocking in transports() {
        tokio::time::timeout(IDENTITY_CHECK_INTERVAL + Duration::from_secs(3), async {
            let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
            publish(&manager, |registry| {
                registry.replace_instance_authorities(vec![InstanceAuthority {
                    root: "a.local".parse().unwrap(),
                    target: InstanceTarget::Current,
                }]);
            });
            let expected = snapshot(&manager);
            for boundary in ["eof", "shutdown", "physical_loss"] {
                let (client, mut peer) = connection(blocking);
                let shutdown = CancellationToken::new();
                let (_state, vm) = watch::channel(VmLifecycleState::Running);
                let after_ack = Arc::new(AtomicBool::new(false));
                let valid = Arc::new(AtomicBool::new(true));
                let checked = Arc::new(tokio::sync::Notify::new());
                let task = tokio::spawn({
                    let manager = Arc::clone(&manager);
                    let shutdown = shutdown.clone();
                    let after_ack = Arc::clone(&after_ack);
                    let valid = Arc::clone(&valid);
                    let checked = Arc::clone(&checked);
                    async move {
                        connected(client, &manager, &shutdown, vm, || {
                            ensure!(valid.load(Ordering::Acquire), "VM incarnation changed");
                            if after_ack.load(Ordering::Acquire) {
                                checked.notify_one();
                            }
                            Ok(())
                        })
                        .await
                    }
                });
                admit(&mut peer).await;
                open(&mut peer).await;
                let received: ReplaceInstanceAuthorityRequest =
                    receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest).await;
                assert_eq!(received, expected);
                after_ack.store(true, Ordering::Release);
                acknowledge(&mut peer, &received).await;
                checked.notified().await;
                match boundary {
                    "shutdown" => shutdown.cancel(),
                    "physical_loss" => {
                        valid.store(false, Ordering::Release);
                        tokio::time::pause();
                        tokio::time::advance(IDENTITY_CHECK_INTERVAL + Duration::from_millis(1))
                            .await;
                        tokio::time::resume();
                    }
                    _ => peer.shutdown().await.unwrap(),
                }
                let result = task.await.unwrap();
                if boundary == "physical_loss" {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("VM incarnation changed")
                    );
                } else {
                    assert_eq!(result.is_err(), boundary == "eof");
                }
                assert!(read_framed(&mut peer).await.is_err());
            }
        })
        .await
        .expect("idle disconnect or complete replay did not finish");
    }
}
