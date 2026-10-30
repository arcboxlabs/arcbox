//! Exercises ALPN dispatch, request authority, and route retirement over real TLS sockets.

use super::*;
use std::sync::Arc;

use arcbox_dns::{
    claims::Claim,
    registry::{InstanceAuthority, InstanceTarget},
};
use http_body_util::{BodyExt as _, Empty};
use tokio::net::TcpListener;
use tokio_rustls::{
    TlsAcceptor, TlsConnector, rustls,
    rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use tokio_util::sync::CancellationToken;

struct Server {
    dns: crate::dns_server::DnsRegistry,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Server {
    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap().unwrap();
    }
}

async fn connect(alpn: Option<&[u8]>) -> (tokio_rustls::client::TlsStream<TcpStream>, Server) {
    let mut config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(
            include_bytes!("../testdata/probe-cert.der").to_vec(),
        )],
        PrivatePkcs8KeyDer::from(include_bytes!("../testdata/probe-key.der").to_vec()).into(),
    )
    .unwrap();
    config.alpn_protocols = alpn.into_iter().map(<[u8]>::to_vec).collect();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let dns = crate::dns_server::DnsRegistry::default();
    let apex: Domain = "arcbox.local".parse().unwrap();
    {
        let mut registry = dns.write().unwrap();
        registry
            .replace(
                "system:index".into(),
                vec![Claim::exclusive(
                    NamePattern::Exact(apex.clone()),
                    "system:index".into(),
                    "index".into(),
                )],
                Some(Ipv4Addr::LOCALHOST.into()),
            )
            .unwrap();
        registry.replace_instance_authorities(vec![InstanceAuthority {
            root: apex,
            target: InstanceTarget::Current,
        }]);
    }
    let services = WebServices::new(dns.clone());
    let route = services.index_route(Ipv4Addr::LOCALHOST).unwrap();
    let shutdown = CancellationToken::new();
    let cancelled = shutdown.clone();
    let task = tokio::spawn(async move {
        super::super::http_tasks::run(&cancelled, async move |executor| {
            let tls = TlsAcceptor::from(Arc::new(config))
                .accept(listener.accept().await?.0)
                .await?;
            serve_tls(tls, services, route, executor).await
        })
        .await
    });
    let client = TlsConnector::from(super::super::probe::configuration())
        .connect(
            "arcbox.local".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(client.get_ref().1.alpn_protocol(), alpn);
    (
        client,
        Server {
            dns,
            shutdown,
            task,
        },
    )
}

#[tokio::test]
async fn tls_h2_validates_each_authority_and_rejects_retired_routes() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for invalidate in ["authority", "registration"] {
            let (client, server) = connect(Some(b"h2")).await;
            let executor = Executor::default();
            let (mut requests, driver) =
                hyper::client::conn::http2::handshake(executor.clone(), TokioIo::new(client))
                    .await
                    .unwrap();
            let driver = tokio::spawn(driver);
            for (authority, hosts, expected) in [
                ("arcbox.local", &[][..], StatusCode::OK),
                ("ARCBOX.LOCAL:443", &["arcbox.local"][..], StatusCode::OK),
                ("arcbox.local", &["arcbox.local:443"][..], StatusCode::OK),
                ("arcbox.local:80", &[][..], StatusCode::MISDIRECTED_REQUEST),
                (
                    "web.arcbox.local",
                    &["arcbox.local"][..],
                    StatusCode::MISDIRECTED_REQUEST,
                ),
                (
                    "arcbox.local",
                    &["other.local"][..],
                    StatusCode::MISDIRECTED_REQUEST,
                ),
                (
                    "arcbox.local",
                    &["arcbox.local", "arcbox.local"][..],
                    StatusCode::MISDIRECTED_REQUEST,
                ),
            ] {
                let mut request = Request::get(format!("https://{authority}/"))
                    .body(Empty::<Bytes>::new())
                    .unwrap();
                for host in hosts {
                    request
                        .headers_mut()
                        .append(header::HOST, host.parse().unwrap());
                }
                let response = requests.send_request(request).await.unwrap();
                assert_eq!(response.version(), Version::HTTP_2);
                assert_eq!(response.status(), expected, "{authority}, {hosts:?}");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                if expected == StatusCode::OK {
                    assert!(String::from_utf8_lossy(&body).contains("directory is unavailable"));
                }
            }
            {
                let mut registry = server.dns.write().unwrap();
                if invalidate == "authority" {
                    registry.invalidate_instance_authorities();
                } else {
                    let previous = registry.get("system:index").unwrap().clone();
                    registry.remove("system:index");
                    registry
                        .replace("system:index".into(), previous.claims, previous.address)
                        .unwrap();
                }
            }
            let response = requests
                .send_request(
                    Request::get("https://arcbox.local/")
                        .body(Empty::<Bytes>::new())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{invalidate}"
            );
            response.into_body().collect().await.unwrap();
            server.stop().await;
            driver.abort();
            // Server shutdown can finish the client transport before abort reaches its driver.
            if let Err(error) = driver.await {
                assert!(error.is_cancelled());
            }
            drop(requests);
            executor.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tls_without_h2_uses_http1_and_the_https_authority_port() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for alpn in [Some(&b"http/1.1"[..]), None] {
            let (client, server) = connect(alpn).await;
            let (mut requests, driver) =
                hyper::client::conn::http1::handshake(TokioIo::new(client))
                    .await
                    .unwrap();
            let driver = tokio::spawn(driver);
            for (host, expected) in [
                ("arcbox.local:443", StatusCode::OK),
                ("arcbox.local:80", StatusCode::MISDIRECTED_REQUEST),
            ] {
                let response = requests
                    .send_request(
                        Request::get("/")
                            .header(header::HOST, host)
                            .body(Empty::<Bytes>::new())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.version(), Version::HTTP_11);
                assert_eq!(response.status(), expected);
                response.into_body().collect().await.unwrap();
            }
            server.stop().await;
            driver.abort();
            if let Err(error) = driver.await {
                assert!(error.is_cancelled());
            }
            drop(requests);
        }
    })
    .await
    .unwrap();
}
