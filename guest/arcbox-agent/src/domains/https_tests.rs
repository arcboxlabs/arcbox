use super::*;
use crate::dns_server::ContainerRegistry;
use arcbox_dns::container::ContainerMetadata;
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;

const NAME: &str = "web.arcbox.local";

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .unwrap()
}

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

async fn publish(
    services: &WebServices,
    dns: &ContainerRegistry,
    owner: &str,
    port: u16,
    domain: &str,
) {
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
        .await
        .replace(
            owner.into(),
            metadata.claims(&domain, "container").claims,
            Some(Ipv4Addr::LOCALHOST.into()),
        )
        .unwrap();
    services.publish(
        &super::super::ContainerFacts::from_inspect(&inspect, &domain).unwrap(),
        super::super::SelectedRoutes {
            http: Some(port),
            https: Some(super::super::HttpsBackend::Terminate(port)),
        },
    );
}

async fn replace_owner(services: &WebServices, dns: &ContainerRegistry, port: u16) {
    dns.write().await.remove("a");
    publish(services, dns, "b", port, "arcbox.local").await;
}

async fn head(stream: &mut (impl AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn owned_foreign_sni_cannot_request_authorization_or_a_certificate() {
    within(async {
        let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = incoming.local_addr().unwrap();
        let dns = ContainerRegistry::default();
        let services = WebServices::new(dns.clone());
        publish(&services, &dns, "a", 3000, "containers.test").await;
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
    })
    .await;
}

#[tokio::test]
async fn owner_replacement_at_each_tls_stage_stops_application_forwarding() {
    for stage in [
        "configuration",
        "handshake",
        "connect",
        "reinstalled",
        "valid",
    ] {
        within(async {
            let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let incoming_address = incoming.local_addr().unwrap();
            let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = backend.local_addr().unwrap().port();
            let dns = ContainerRegistry::default();
            let services = WebServices::new(dns.clone());
            publish(&services, &dns, "a", port, "arcbox.local").await;
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
                            Ok(configuration())
                        },
                        async move |address: (Ipv4Addr, u16)| {
                            assert_eq!(address, (Ipv4Addr::LOCALHOST, port));
                            let stream = TcpStream::connect(address).await?;
                            connected.send(()).unwrap();
                            connection_released.await.unwrap();
                            Ok(stream)
                        },
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
                replace_owner(&services, &dns, port).await;
            }
            configure.send(()).unwrap();
            if stage == "handshake" {
                // Server flight proves configuration passed; withholding client polls pauses TLS.
                assert_ne!(
                    handshake.get_ref().unwrap().peek(&mut [0]).await.unwrap(),
                    0
                );
                replace_owner(&services, &dns, port).await;
            }
            if stage == "configuration" {
                let error = relay.await.unwrap().unwrap_err();
                assert!(format!("{error:#}").contains("changed during the handshake"));
                assert!(handshake.await.is_err());
                assert!(matches!(
                    connection_started.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
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
                return;
            }
            connection_started.await.unwrap();
            let mut upstream = backend.accept().await.unwrap().0;
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: web.arcbox.local\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            if stage == "connect" {
                replace_owner(&services, &dns, port).await;
            } else if stage == "reinstalled" {
                let revision = dns.read().await.get("a").unwrap().revision;
                services.remove("a");
                publish(&services, &dns, "a", port, "arcbox.local").await;
                assert_eq!(dns.read().await.get("a").unwrap().revision, revision);
            }
            connect.send(()).unwrap();
            if matches!(stage, "connect" | "reinstalled") {
                let error = relay.await.unwrap().unwrap_err();
                assert!(format!("{error:#}").contains("changed during the handshake"));
                assert_eq!(upstream.read(&mut [0]).await.unwrap(), 0);
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
