use super::*;
#[cfg(target_os = "linux")]
use crate::agent::{AuthorityPeer, Services};
use crate::dns_server::DnsRegistry;
#[cfg(target_os = "linux")]
use arcbox_connect::v1::{InstanceAuthorityRoot, ReplaceInstanceAuthorityRequest};
use arcbox_dns::container::ContainerMetadata;
use std::future::Future;
#[cfg(target_os = "linux")]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;

#[cfg(target_os = "linux")]
#[path = "https_http2_tests.rs"]
mod http2;

#[tokio::test]
async fn frontend_bindings_return_distinct_available_ports() {
    within(async {
        let (first, first_port) = Https::bind_listener().await.unwrap();
        let (second, second_port) = Https::bind_listener().await.unwrap();
        assert_ne!(first_port, 0);
        assert_ne!(second_port, 0);
        assert_ne!(first_port, second_port);
        assert_eq!(first.local_addr().unwrap().port(), first_port);
        assert_eq!(second.local_addr().unwrap().port(), second_port);
    })
    .await;
}

fn publish_index(dns: &DnsRegistry) {
    use arcbox_dns::{
        NamePattern,
        claims::Claim,
        registry::{InstanceAuthority, InstanceTarget},
    };
    let apex: arcbox_constants::dns::Domain = "arcbox.local".parse().unwrap();
    let mut dns = dns.write().unwrap();
    dns.replace_instance_authorities(vec![InstanceAuthority {
        root: apex.clone(),
        target: InstanceTarget::Current,
    }]);
    dns.replace(
        "system:index".into(),
        vec![Claim::exclusive(
            NamePattern::Exact(apex),
            "system:index".into(),
            "index".into(),
        )],
        Some(Ipv4Addr::LOCALHOST.into()),
    )
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn index_authorization_wait_rechecks_exact_registration() {
    let dns = DnsRegistry::default();
    let services = WebServices::new(dns.clone());
    publish_index(&dns);
    let mut request = arcbox_connect::v1::ReconcileHttpsAccessRequest {
        observation_id: "index".into(),
        host_addresses: vec![Ipv4Addr::LOCALHOST.to_string()],
        enabled: true,
        ..Default::default()
    };
    services.reconcile_https_access(request.clone()).unwrap();
    assert!(
        services
            .observe_managed_tls(
                "child.arcbox.local",
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::LOCALHOST.into()
            )
            .await
            .is_err()
    );
    assert_eq!(
        services
            .reconcile_https_access(request.clone())
            .unwrap()
            .sequence,
        0
    );
    let handshake = tokio::spawn({
        let services = services.clone();
        async move {
            services
                .observe_managed_tls(
                    "arcbox.local",
                    Ipv4Addr::LOCALHOST,
                    Ipv4Addr::LOCALHOST.into(),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let access = services.reconcile_https_access(request.clone()).unwrap();
    assert_eq!(access.hostname, "arcbox.local");
    assert_eq!(access.sequence, 1);
    dns.write().unwrap().remove("system:index");
    publish_index(&dns);
    request.enabled = false;
    services.reconcile_https_access(request).unwrap();
    assert!(handshake.await.unwrap().is_err());
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn index_tls_serves_html_without_upstream_and_rejects_stale_handshakes() {
    use http_body_util::BodyExt as _;
    use hyper_util::rt::TokioIo;
    for stage in ["valid", "configuration", "handshake"] {
        within(async {
            let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = incoming.local_addr().unwrap();
            let dns = DnsRegistry::default();
            let services = WebServices::new(dns.clone());
            publish_index(&dns);
            let (configured, configuration_started) = oneshot::channel();
            let (configure, configuration_released) = oneshot::channel();
            let relay = tokio::spawn(async move {
                relay_to(
                    incoming.accept().await.unwrap().0,
                    Ipv4Addr::LOCALHOST,
                    &services,
                    async |name: &str| {
                        assert_eq!(name, "arcbox.local");
                        configured.send(()).unwrap();
                        configuration_released.await.unwrap();
                        Ok(configuration())
                    },
                    async |_| panic!("index must not connect to an upstream"),
                    super::super::http_tasks::Executor::default(),
                )
                .await
            });
            let connector = TlsConnector::from(super::super::probe::configuration());
            let socket = TcpStream::connect(address).await.unwrap();
            let mut handshake = connector.connect("arcbox.local".try_into().unwrap(), socket);
            tokio::select! {
                result = &mut handshake => panic!("TLS finished before configuration: {result:?}"),
                result = configuration_started => result.unwrap(),
            }
            if stage == "configuration" {
                dns.write().unwrap().remove("system:index");
            }
            configure.send(()).unwrap();
            if stage == "handshake" {
                assert_ne!(
                    handshake.get_ref().unwrap().peek(&mut [0]).await.unwrap(),
                    0
                );
                dns.write().unwrap().remove("system:index");
            }
            if stage == "configuration" {
                assert!(handshake.await.is_err());
            } else {
                let client = handshake.await.unwrap();
                if stage == "valid" {
                    let (mut requests, driver) =
                        hyper::client::conn::http1::handshake(TokioIo::new(client))
                            .await
                            .unwrap();
                    let driver = tokio::spawn(driver);
                    let response = requests
                        .send_request(
                            hyper::Request::builder()
                                .uri("/")
                                .header(hyper::header::HOST, "arcbox.local:443")
                                .body(http_body_util::Full::new(bytes::Bytes::new()))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(response.status(), hyper::StatusCode::OK);
                    assert_eq!(response.headers()[hyper::header::CACHE_CONTROL], "no-store");
                    let body = response.into_body().collect().await.unwrap().to_bytes();
                    let body = String::from_utf8_lossy(&body);
                    assert!(body.contains("Local web services"));
                    assert!(body.contains("directory is unavailable"));
                    drop(requests);
                    driver.await.unwrap().unwrap();
                }
            }
            let result = relay.await.unwrap();
            if stage == "valid" {
                result.unwrap();
            } else {
                assert!(
                    format!("{:#}", result.unwrap_err()).contains("changed during the handshake")
                );
            }
        })
        .await;
    }
}

#[cfg(target_os = "linux")]
const NAME: &str = "web.arcbox.local";

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .unwrap()
}

#[cfg(target_os = "linux")]
fn configuration() -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    Arc::new(
        rustls::ServerConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("testdata/probe-cert.der").to_vec(),
            )],
            PrivatePkcs8KeyDer::from(include_bytes!("testdata/probe-key.der").to_vec()).into(),
        )
        .unwrap(),
    )
}

fn publish(services: &WebServices, dns: &DnsRegistry, owner: &str, port: u16, domain: &str) {
    let domain = domain.parse().unwrap();
    let inspect = serde_json::json!({
        "Id": owner, "Name": "/web", "State": { "Pid": 1 },
        "NetworkSettings": {
            "SandboxKey": "/var/run/docker/netns/web",
            "Networks": { "bridge": { "IPAddress": "127.0.0.1" } }
        }
    });
    let metadata = ContainerMetadata::from_inspect(&inspect).unwrap();
    dns.write()
        .unwrap()
        .replace(
            owner.into(),
            metadata.claims(&domain, "container").claims,
            Some(Ipv4Addr::LOCALHOST.into()),
        )
        .unwrap();
    services.publish(
        &super::super::EndpointFacts::from_inspect(&inspect, &domain).unwrap(),
        super::super::SelectedRoutes {
            http: Some(port),
            https: Some(super::super::HttpsBackend::Terminate(
                super::super::HttpBackend {
                    port,
                    protocol: Some(super::super::HttpProtocol::Http1),
                },
            )),
        },
    );
}

#[cfg(target_os = "linux")]
fn replace_owner(services: &WebServices, dns: &DnsRegistry, port: u16) {
    dns.write().unwrap().remove("a");
    publish(services, dns, "b", port, "arcbox.local");
}

#[cfg(target_os = "linux")]
fn authority_snapshot(authority_revision: u64) -> ReplaceInstanceAuthorityRequest {
    ReplaceInstanceAuthorityRequest {
        authority_revision,
        roots: vec![InstanceAuthorityRoot {
            root: "arcbox.local".into(),
            target: arcbox_connect::v1::instance_authority_root::Target::Current.into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[cfg(target_os = "linux")]
async fn head(stream: &mut (impl AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn owned_foreign_sni_cannot_request_authorization_or_a_certificate() {
    within(Box::pin(async {
        let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = incoming.local_addr().unwrap();
        let dns = DnsRegistry::default();
        let services = WebServices::new(dns.clone());
        publish(&services, &dns, "a", 3000, "containers.test");
        let observation = arcbox_connect::v1::ReconcileHttpsAccessRequest {
            observation_id: "foreign".into(),
            host_addresses: vec![Ipv4Addr::LOCALHOST.to_string()],
            enabled: true,
            ..Default::default()
        };
        services
            .reconcile_https_access(observation.clone())
            .unwrap();
        let server = async {
            let client = incoming.accept().await.unwrap().0;
            relay_to(
                client,
                Ipv4Addr::LOCALHOST,
                &services,
                async |_| panic!("foreign SNI must not request a certificate"),
                async |_| panic!("foreign SNI must not connect upstream"),
                super::super::http_tasks::Executor::default(),
            )
            .await
        };
        let client = async {
            let socket = TcpStream::connect(address).await.unwrap();
            TlsConnector::from(super::super::probe::configuration())
                .connect("web.containers.test".try_into().unwrap(), socket)
                .await
        };
        let (server, client) = tokio::join!(server, client);
        assert!(server.unwrap_err().to_string().contains("no managed route"));
        assert!(client.is_err());
        assert_eq!(
            services
                .reconcile_https_access(observation)
                .unwrap()
                .sequence,
            0
        );
    }))
    .await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn ownership_changes_at_each_tls_stage_stop_application_forwarding() {
    for (stage, mutation) in [
        ("configuration", "resource"),
        ("handshake", "resource"),
        ("connect", "resource"),
        ("reinstalled", "resource"),
        ("valid", "resource"),
        ("configuration", "aba"),
        ("handshake", "aba"),
        ("connect", "aba"),
        ("configuration", "eof"),
        ("handshake", "eof"),
        ("connect", "eof"),
    ] {
        within(async {
            let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let incoming_address = incoming.local_addr().unwrap();
            let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = backend.local_addr().unwrap().port();
            let server = Arc::new(
                crate::dns_server::GuestDnsServer::new(
                    Default::default(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .unwrap(),
            );
            let dns = server.registry();
            let services = WebServices::new(dns.clone());
            publish(&services, &dns, "a", port, "arcbox.local");
            let mut authority = if mutation == "resource" {
                None
            } else {
                let mut peer =
                    AuthorityPeer::open(Services::system(services.clone(), server)).await;
                peer.replace(authority_snapshot(10)).await;
                Some(peer)
            };
            let mut change_owner = async || match mutation {
                "resource" => replace_owner(&services, &dns, port),
                "aba" => {
                    authority
                        .as_mut()
                        .unwrap()
                        .replace(authority_snapshot(12))
                        .await;
                }
                "eof" => {
                    authority.take().unwrap().close().await.unwrap_err();
                }
                _ => unreachable!(),
            };
            let cached =
                super::super::certificates::CachedConfiguration::new(NAME, configuration()).await;
            let (configured, configuration_started) = oneshot::channel();
            let (configure, configuration_released) = oneshot::channel();
            let (connected, mut connection_started) = oneshot::channel();
            let (connect, connection_released) = oneshot::channel();
            let relay = tokio::spawn({
                let services = services.clone();
                async move {
                    let client = incoming.accept().await.unwrap().0;
                    relay_to(
                        client,
                        Ipv4Addr::LOCALHOST,
                        &services,
                        async move |name: &str| {
                            assert_eq!(name, NAME);
                            configured.send(()).unwrap();
                            configuration_released.await.unwrap();
                            cached.configuration(name).await
                        },
                        async move |address: (Ipv4Addr, u16)| {
                            assert_eq!(address, (Ipv4Addr::LOCALHOST, port));
                            let stream = TcpStream::connect(address).await?;
                            connected.send(()).unwrap();
                            connection_released.await.unwrap();
                            Ok(stream)
                        },
                        super::super::http_tasks::Executor::default(),
                    )
                    .await
                }
            });
            let client = TcpStream::connect(incoming_address).await.unwrap();
            let connector = TlsConnector::from(super::super::probe::configuration());
            let mut handshake = connector.connect(NAME.try_into().unwrap(), client);
            tokio::select! {
                result = &mut handshake => panic!("TLS finished before configuration: {result:?}"),
                result = configuration_started => result.unwrap(),
            }
            if stage == "configuration" {
                change_owner().await;
            }
            configure.send(()).unwrap();
            if stage == "handshake" {
                // Server flight proves configuration passed; withholding client polls pauses TLS.
                assert_ne!(
                    handshake.get_ref().unwrap().peek(&mut [0]).await.unwrap(),
                    0
                );
                change_owner().await;
            }
            if stage == "configuration" {
                let error = relay.await.unwrap().unwrap_err();
                assert!(format!("{error:#}").contains("changed during the handshake"));
                assert!(handshake.await.is_err());
                assert!(matches!(
                    connection_started.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
                drop(change_owner);
                if let Some(peer) = authority.take() {
                    peer.close().await.unwrap_err();
                }
                return;
            }
            let mut client = handshake.await.unwrap();
            if stage == "handshake" {
                let error = relay.await.unwrap().unwrap_err();
                assert!(format!("{error:#}").contains("changed during the handshake"));
                assert!(matches!(
                    connection_started.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
                drop(change_owner);
                if let Some(peer) = authority.take() {
                    peer.close().await.unwrap_err();
                }
                return;
            }
            connection_started.await.unwrap();
            let mut upstream = backend.accept().await.unwrap().0;
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: web.arcbox.local\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            if stage == "connect" {
                change_owner().await;
            } else if stage == "reinstalled" {
                let revision = dns.read().unwrap().get("a").unwrap().revision;
                services.remove("a");
                publish(&services, &dns, "a", port, "arcbox.local");
                assert_eq!(dns.read().unwrap().get("a").unwrap().revision, revision);
            }
            drop(change_owner);
            connect.send(()).unwrap();
            if matches!(stage, "connect" | "reinstalled") {
                let error = relay.await.unwrap().unwrap_err();
                assert!(format!("{error:#}").contains("changed during the handshake"));
                assert_eq!(upstream.read(&mut [0]).await.unwrap(), 0);
                if let Some(peer) = authority.take() {
                    peer.close().await.unwrap_err();
                }
                return;
            }
            let request = head(&mut upstream).await.to_ascii_lowercase();
            assert!(request.starts_with("get / http/1.1\r\n"));
            assert!(request.contains("x-forwarded-proto: https\r\n"));
            upstream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            upstream.shutdown().await.unwrap();
            assert!(head(&mut client).await.starts_with("HTTP/1.1 200"));
            let mut body = [0; 2];
            client.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"ok");
            drop(client);
            drop(upstream);
            relay.await.unwrap().unwrap();
        })
        .await;
    }
}
