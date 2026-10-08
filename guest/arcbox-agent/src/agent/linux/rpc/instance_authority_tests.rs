//! Framed socket coverage for exclusive instance authority ownership.

use super::*;
use crate::agent::Services;
use crate::dns_server::ContainerRegistry;
use crate::rpc::MessageType;
use arcbox_connect::v1::instance_authority_root::Target;
use arcbox_connect::v1::{
    InstanceAuthorityAck, InstanceAuthorityRoot, ReplaceInstanceAuthorityRequest,
};
use arcbox_dns::registry::{InstanceTarget, Lookup};
use tokio::io::AsyncWriteExt as _;

/// A real framed host peer for guest authority and TLS regressions.
pub struct AuthorityPeer {
    client: tokio::net::UnixStream,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl AuthorityPeer {
    async fn response(&mut self) -> Result<(MessageType, String, Vec<u8>)> {
        tokio::time::timeout(Duration::from_secs(3), read_message(&mut self.client))
            .await
            .expect("authority response timed out")
    }

    fn spawn(services: Services, guest: Guest, cid: u32) -> Self {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let task =
            tokio::spawn(async move { handle_connection(server, guest, &services, cid).await });
        Self { client, task }
    }

    async fn request_open(&mut self) -> (MessageType, String, Vec<u8>) {
        write_message(
            &mut self.client,
            MessageType::OpenInstanceAuthorityRequest,
            "open",
            &[],
        )
        .await
        .unwrap();
        self.response().await.unwrap()
    }

    /// Opens the exclusive authority channel through the production dispatcher.
    pub async fn open(services: Services) -> Self {
        let mut peer = Self::spawn(services, Guest::SystemVm, tokio_vsock::VMADDR_CID_HOST);
        assert_eq!(
            peer.request_open().await.0,
            MessageType::OpenInstanceAuthorityResponse
        );
        peer
    }

    async fn send(&mut self, request: &ReplaceInstanceAuthorityRequest) {
        write_message(
            &mut self.client,
            MessageType::ReplaceInstanceAuthorityRequest,
            "replace",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    }

    /// Applies a complete source snapshot and verifies its committed acknowledgement.
    pub async fn replace(&mut self, request: ReplaceInstanceAuthorityRequest) {
        self.send(&request).await;
        let (kind, trace, payload) = self.response().await.unwrap();
        assert_eq!(kind, MessageType::InstanceAuthorityAck);
        assert_eq!(trace, "replace");
        assert_eq!(
            InstanceAuthorityAck::decode_from_slice(&payload)
                .unwrap()
                .authority_revision,
            request.authority_revision
        );
    }

    /// Closes the peer and joins the dispatcher before returning.
    pub async fn close(self) -> Result<()> {
        drop(self.client);
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

fn services(registry: &ContainerRegistry) -> Services {
    Services::system(
        crate::domains::WebServices::new(registry.clone()),
        registry.clone(),
    )
}

fn replacement(revision: u64) -> ReplaceInstanceAuthorityRequest {
    ReplaceInstanceAuthorityRequest {
        authority_revision: revision,
        roots: vec![InstanceAuthorityRoot {
            root: "ARCBOX.local".into(),
            target: Target::Current.into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn instance(registry: &ContainerRegistry) -> (InstanceTarget, u64) {
    match registry
        .read()
        .unwrap()
        .resolve(&"web.arcbox.local".parse().unwrap())
    {
        Lookup::Instance {
            authority,
            revision,
        } => (authority.target, revision),
        result => panic!("expected instance root, got {result:?}"),
    }
}

#[tokio::test]
async fn snapshot_is_committed_before_ack_and_eof_revokes_known_roots() {
    let registry = ContainerRegistry::default();
    let mut peer = AuthorityPeer::open(services(&registry)).await;
    peer.replace(replacement(0)).await;
    assert_eq!(instance(&registry).0, InstanceTarget::Current);
    peer.close().await.unwrap_err();
    assert_eq!(instance(&registry).0, InstanceTarget::Unavailable);
}

#[tokio::test]
async fn only_the_host_on_the_system_vm_can_open_authority() {
    for (guest, cid, configured) in [
        (Guest::SystemVm, 3, true),
        (Guest::DistroMachine, 2, true),
        (Guest::StorageRecovery, 2, true),
        (Guest::SystemVm, 2, false),
    ] {
        let registry = ContainerRegistry::default();
        let context = if configured {
            services(&registry)
        } else {
            Services::default()
        };
        let mut peer = AuthorityPeer::spawn(context, guest, cid);
        let (kind, _, payload) = peer.request_open().await;
        assert_eq!(kind, MessageType::Error);
        assert_eq!(ErrorResponse::decode(&payload).unwrap().code, 403);
        peer.close().await.unwrap();
        assert_eq!(registry.read().unwrap().epoch(), 0);
    }
}

#[tokio::test]
async fn exclusive_handoff_resets_source_revision_after_withdrawal() {
    let registry = ContainerRegistry::default();
    let services = services(&registry);
    let mut owner = AuthorityPeer::open(services.clone()).await;
    owner.replace(replacement(10)).await;
    let current = instance(&registry);
    let mut second = AuthorityPeer::spawn(services.clone(), Guest::SystemVm, 2);
    let (kind, _, payload) = second.request_open().await;
    assert_eq!(kind, MessageType::Error);
    assert_eq!(ErrorResponse::decode(&payload).unwrap().code, 409);
    second.close().await.unwrap();
    assert_eq!(instance(&registry), current);
    owner.close().await.unwrap_err();
    let withdrawn = instance(&registry);
    assert_eq!(withdrawn.0, InstanceTarget::Unavailable);
    let mut next = AuthorityPeer::open(services).await;
    assert_eq!(instance(&registry), withdrawn);
    next.replace(replacement(0)).await;
    assert_eq!(instance(&registry).0, InstanceTarget::Current);
    assert!(instance(&registry).1 > current.1);
    let mut release = replacement(1);
    release.roots.clear();
    next.replace(release).await;
    assert!(matches!(
        registry
            .read()
            .unwrap()
            .resolve(&"web.arcbox.local".parse().unwrap()),
        Lookup::Unmatched
    ));
    let released_epoch = registry.read().unwrap().epoch();
    next.close().await.unwrap_err();
    assert!(registry.read().unwrap().epoch() > released_epoch);
}

#[tokio::test]
async fn invalid_replacements_close_the_connection_and_retain_unavailable_roots() {
    for case in [
        "equal",
        "backward",
        "name",
        "duplicate",
        "unknown",
        "unspecified",
        "current-port",
        "unavailable-port",
        "other-zero",
        "other-large",
    ] {
        let registry = ContainerRegistry::default();
        let mut peer = AuthorityPeer::open(services(&registry)).await;
        peer.replace(replacement(10)).await;
        let mut invalid = replacement(11);
        match case {
            "equal" => invalid.authority_revision = 10,
            "backward" => invalid.authority_revision = 9,
            "name" => invalid.roots[0].root = "*.arcbox.local".into(),
            "duplicate" => {
                let mut duplicate = invalid.roots[0].clone();
                duplicate.root.make_ascii_lowercase();
                invalid.roots.push(duplicate);
            }
            "unknown" => invalid.roots[0].target = buffa::EnumValue::Unknown(42),
            "unspecified" => invalid.roots[0].target = Target::Unspecified.into(),
            "current-port" => invalid.roots[0].dns_port = 53,
            "unavailable-port" => {
                invalid.roots[0].target = Target::Unavailable.into();
                invalid.roots[0].dns_port = 53;
            }
            "other-zero" | "other-large" => {
                invalid.roots[0].target = Target::Other.into();
                invalid.roots[0].dns_port = if case == "other-zero" { 0 } else { 65536 };
            }
            _ => unreachable!(),
        }
        peer.send(&invalid).await;
        assert!(peer.response().await.is_err(), "{case}");
        peer.close().await.unwrap_err();
        assert_eq!(instance(&registry).0, InstanceTarget::Unavailable, "{case}");
    }
}

#[tokio::test]
async fn coalesced_source_lifetimes_invalidate_identical_and_empty_snapshots() {
    let registry = ContainerRegistry::default();
    let mut peer = AuthorityPeer::open(services(&registry)).await;
    for (revision, target, port) in [
        (10, Target::Current, 0),
        (20, Target::Other, 5354),
        (30, Target::Unavailable, 0),
    ] {
        let mut snapshot = replacement(revision);
        snapshot.roots[0].target = target.into();
        snapshot.roots[0].dns_port = port;
        peer.replace(snapshot.clone()).await;
        let before = instance(&registry);
        assert_eq!(
            before.0,
            match target {
                Target::Current => InstanceTarget::Current,
                Target::Other => InstanceTarget::Other { port: 5354 },
                Target::Unavailable => InstanceTarget::Unavailable,
                Target::Unspecified => unreachable!(),
            }
        );
        snapshot.authority_revision += 2;
        peer.replace(snapshot).await;
        let after = instance(&registry);
        assert_eq!(after.0, before.0);
        assert!(after.1 > before.1);
    }
    for revision in [40, 42] {
        let before = registry.read().unwrap().epoch();
        let mut empty = replacement(revision);
        empty.roots.clear();
        peer.replace(empty).await;
        assert!(registry.read().unwrap().epoch() > before);
    }
    peer.close().await.unwrap_err();
}

#[tokio::test]
async fn replacement_without_open_cannot_mutate_the_registry() {
    let registry = ContainerRegistry::default();
    let mut peer = AuthorityPeer::spawn(services(&registry), Guest::SystemVm, 2);
    peer.send(&replacement(0)).await;
    assert!(peer.response().await.is_err());
    peer.close().await.unwrap_err();
    assert_eq!(registry.read().unwrap().epoch(), 0);
}

#[tokio::test]
async fn malformed_and_unexpected_frames_revoke_the_active_snapshot() {
    for raw in [
        &[0, 0, 0, 3, 0, 0, 0][..],
        &[0, 0, 0, 6, 0, 0, 0, 25, 0, 1],
        &[0, 0, 0, 7, 0, 0, 0, 25, 0, 1, 0xff],
        &[0xff, 0xff, 0xff, 0xff],
        &[0, 0, 0, 6, 0, 0],
        &[0, 0, 0, 6, 0, 0, 0, 1, 0, 0],
    ] {
        let registry = ContainerRegistry::default();
        let mut peer = AuthorityPeer::open(services(&registry)).await;
        peer.replace(replacement(1)).await;
        peer.client.write_all(raw).await.unwrap();
        peer.client.shutdown().await.unwrap();
        assert!(peer.response().await.is_err());
        peer.close().await.unwrap_err();
        assert_eq!(instance(&registry).0, InstanceTarget::Unavailable);
    }
}

#[tokio::test]
async fn cancellation_during_partial_read_or_ack_revokes_before_the_next_open() {
    for phase in ["partial-frame", "ack"] {
        let registry = ContainerRegistry::default();
        let services = services(&registry);
        let (mut client, server) = tokio::io::duplex(1);
        let task = tokio::spawn({
            let services = services.clone();
            async move { handle_connection(server, Guest::SystemVm, &services, 2).await }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            write_message(
                &mut client,
                MessageType::OpenInstanceAuthorityRequest,
                "",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(
                read_message(&mut client).await.unwrap().0,
                MessageType::OpenInstanceAuthorityResponse
            );
            write_message(
                &mut client,
                MessageType::ReplaceInstanceAuthorityRequest,
                "",
                &replacement(10).encode_to_vec(),
            )
            .await
            .unwrap();
            if phase == "partial-frame" {
                assert_eq!(
                    read_message(&mut client).await.unwrap().0,
                    MessageType::InstanceAuthorityAck
                );
                // A one-byte buffer proves the server consumed part of this incomplete header.
                client.write_all(&[0, 0]).await.unwrap();
            } else {
                // Reading one byte proves commit; the remaining ACK cannot fit in the buffer.
                client.read_exact(&mut [0]).await.unwrap();
            }
            assert_eq!(instance(&registry).0, InstanceTarget::Current);
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(instance(&registry).0, InstanceTarget::Unavailable);
            let mut next = AuthorityPeer::open(services).await;
            assert_eq!(instance(&registry).0, InstanceTarget::Unavailable);
            next.replace(replacement(0)).await;
            assert_eq!(instance(&registry).0, InstanceTarget::Current);
            next.close().await.unwrap_err();
        })
        .await
        .expect("authority cancellation exceeded the test deadline");
    }
}
