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

#[tokio::test]
async fn snapshot_is_committed_before_ack_and_eof_revokes_known_roots() {
    let registry = ContainerRegistry::default();
    let services = Services::system(
        crate::domains::WebServices::new(registry.clone()),
        registry.clone(),
    );
    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        handle_connection(
            server,
            Guest::SystemVm,
            &services,
            tokio_vsock::VMADDR_CID_HOST,
        )
        .await
    });
    write_message(
        &mut client,
        MessageType::OpenInstanceAuthorityRequest,
        "open",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        read_message(&mut client).await.unwrap().0,
        MessageType::OpenInstanceAuthorityResponse
    );
    let request = ReplaceInstanceAuthorityRequest {
        authority_revision: 0,
        roots: vec![InstanceAuthorityRoot {
            root: "ARCBOX.local".into(),
            target: Target::Current.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    write_message(
        &mut client,
        MessageType::ReplaceInstanceAuthorityRequest,
        "replace",
        &request.encode_to_vec(),
    )
    .await
    .unwrap();
    let (kind, trace, payload) = read_message(&mut client).await.unwrap();
    assert_eq!(kind, MessageType::InstanceAuthorityAck);
    assert_eq!(trace, "replace");
    assert_eq!(
        InstanceAuthorityAck::decode_from_slice(&payload)
            .unwrap()
            .authority_revision,
        0
    );
    let name = "web.arcbox.local".parse().unwrap();
    assert!(
        matches!(registry.read().unwrap().resolve(&name), Lookup::Instance { authority, .. } if authority.target == InstanceTarget::Current)
    );
    drop(client);
    task.await.unwrap().unwrap_err();
    assert!(
        matches!(registry.read().unwrap().resolve(&name), Lookup::Instance { authority, .. } if authority.target == InstanceTarget::Unavailable)
    );
}
