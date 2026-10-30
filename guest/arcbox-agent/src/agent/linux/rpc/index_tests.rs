//! The built-in index uses the same framed directory RPC as container routes.

use super::*;
use crate::domains::WebServices;
use crate::rpc::MessageType;
use arcbox_connect::v1::{
    ListGuestWebServicesRequest, ListGuestWebServicesResponse, guest_web_service::Route,
};
use arcbox_dns::{
    NamePattern,
    claims::Claim,
    registry::{InstanceAuthority, InstanceTarget},
};

async fn directory(
    client: &mut tokio::io::DuplexStream,
    hostname: &str,
) -> ListGuestWebServicesResponse {
    write_message(
        client,
        MessageType::ListGuestWebServicesRequest,
        "index",
        &ListGuestWebServicesRequest {
            hostname: hostname.into(),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await
    .unwrap();
    let (kind, trace, payload) = read_message(client).await.unwrap();
    assert_eq!(kind, MessageType::ListGuestWebServicesResponse);
    assert_eq!(trace, "index");
    ListGuestWebServicesResponse::decode_from_slice(&payload).unwrap()
}

#[tokio::test]
async fn current_index_is_typed_exact_and_independent_of_docker_directory() {
    for (apex, managed_https) in [("arcbox.local", true), ("containers.test", false)] {
        tokio::time::timeout(Duration::from_secs(3), async {
            let dns = std::sync::Arc::new(
                crate::dns_server::GuestDnsServer::new(
                    apex.parse().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .unwrap(),
            );
            let registry = dns.registry();
            let web = WebServices::new(registry.clone());
            let claims = vec![Claim::exclusive(
                NamePattern::Exact(apex.parse().unwrap()),
                "system:index".into(),
                "index".into(),
            )];
            let publish = || {
                let mut dns = registry.write().unwrap();
                dns.replace_instance_authorities(vec![InstanceAuthority {
                    root: apex.parse().unwrap(),
                    target: InstanceTarget::Current,
                }]);
                dns.replace(
                    "system:index".into(),
                    claims.clone(),
                    Some("172.17.0.1".parse().unwrap()),
                )
                .unwrap();
            };
            let services = crate::agent::Services::system(web.clone(), dns);
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(async move {
                handle_connection(
                    server,
                    Guest::SystemVm,
                    &services,
                    tokio_vsock::VMADDR_CID_HOST,
                )
                .await
            });
            assert_eq!(directory(&mut client, "").await.services, []);
            publish();
            for (hostname, expected) in [
                (String::new(), true),
                (apex.to_uppercase(), true),
                (format!("child.{apex}"), false),
            ] {
                let response = directory(&mut client, &hostname).await;
                assert_eq!(response.services.len(), usize::from(expected), "{hostname}");
                if expected {
                    let Some(Route::Index(index)) = response.services[0].route.as_ref() else {
                        panic!("expected typed index route")
                    };
                    assert_eq!(index.hostname, apex);
                    assert_eq!(index.ipv4_address, "172.17.0.1");
                    assert_eq!(index.managed_https, managed_https);
                }
            }
            let source = web.directory_source();
            source.ready();
            drop(source);
            assert_eq!(directory(&mut client, apex).await.services.len(), 1);
            registry.write().unwrap().remove("system:index");
            assert_eq!(directory(&mut client, apex).await.services, []);
            publish();
            registry.write().unwrap().invalidate_instance_authorities();
            assert_eq!(directory(&mut client, apex).await.services, []);
            drop(client);
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}
