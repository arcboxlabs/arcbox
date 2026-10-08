use super::*;
use arcbox_connect::v1::{AgentPingResponse, InstanceAuthorityAck, OpenInstanceAuthorityResponse};
use arcbox_constants::wire::{AGENT_PROTOCOL_VERSION, MessageType};
use arcbox_dns::{claims::Claim, registry::InstanceAuthority};
use arcbox_transport::vsock::read_framed;
use buffa::Message;
use std::os::fd::IntoRawFd;
use tokio::{io::AsyncWriteExt as _, net::UnixStream};

async fn receive<T: Message>(peer: &mut UnixStream, expected: MessageType) -> T {
    let frame = read_framed(peer).await.unwrap();
    assert_eq!(&frame[4..8], &(expected as u32).to_be_bytes());
    assert_eq!(&frame[8..10], &[0, 0]);
    T::decode_from_slice(&frame[10..]).unwrap()
}

async fn respond(peer: &mut UnixStream, kind: MessageType, message: &impl Message) {
    peer.write_all(&AgentClient::build_message(kind, "", &message.encode_to_vec()))
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

#[tokio::test]
async fn delivery_sends_complete_authority_and_preserves_source_lifetimes() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (host, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = AgentClient::from_fd_blocking(3, host.into_raw_fd()).unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut peer = UnixStream::from_std(peer).unwrap();
        let manager = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
        let shutdown = CancellationToken::new();
        let (_state, vm) = watch::channel(VmLifecycleState::Stopped);
        let task = tokio::spawn({
            let manager = Arc::clone(&manager);
            let shutdown = shutdown.clone();
            async move { connected(client, &manager, &shutdown, vm, || Ok(())).await }
        });
        let _: arcbox_connect::v1::AgentPingRequest =
            receive(&mut peer, MessageType::PingRequest).await;
        respond(
            &mut peer,
            MessageType::PingResponse,
            &AgentPingResponse {
                protocol_version: AGENT_PROTOCOL_VERSION,
                ..Default::default()
            },
        )
        .await;
        let _: arcbox_connect::v1::OpenInstanceAuthorityRequest =
            receive(&mut peer, MessageType::OpenInstanceAuthorityRequest).await;
        respond(
            &mut peer,
            MessageType::OpenInstanceAuthorityResponse,
            &OpenInstanceAuthorityResponse::default(),
        )
        .await;
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
