//! Protocol discovery against the container address, outside PREROUTING NAT.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Empty;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, rustls};

use super::http_tasks::Executor;
use super::selection::HttpProtocol;

const TIMEOUT: Duration = Duration::from_secs(1);

/// Cache only successful probes until socket identities or container facts change.
#[derive(Default)]
pub(super) struct Cache {
    listeners: Option<super::listeners::Snapshot>,
    https: BTreeSet<u16>,
    http: BTreeMap<u16, HttpProtocol>,
}

impl Cache {
    pub(super) fn observe(&mut self, listeners: &super::listeners::Snapshot) {
        if self.listeners.as_ref() != Some(listeners) {
            self.invalidate();
            self.listeners = Some(listeners.clone());
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.https.clear();
        self.http.clear();
    }

    pub(super) async fn responds_https(&mut self, ip: Ipv4Addr, port: u16, name: &str) -> bool {
        if self.https.contains(&port) {
            return true;
        }
        if responds_https(ip, port, name).await {
            self.https.insert(port);
            return true;
        }
        false
    }

    pub(super) async fn http(
        &mut self,
        ip: Ipv4Addr,
        port: u16,
        name: &str,
    ) -> Option<HttpProtocol> {
        if let Some(protocol) = self.http.get(&port) {
            return Some(*protocol);
        }
        let executor = Executor::default();
        let protocol = http(ip, port, name, executor.clone()).await;
        executor.shutdown().await;
        let protocol = protocol?;
        self.http.insert(port, protocol);
        Some(protocol)
    }
}

/// Prefer HTTP/2; each protocol probe owns a separate bounded connection.
pub(super) async fn http(
    ip: Ipv4Addr,
    port: u16,
    name: &str,
    executor: Executor,
) -> Option<HttpProtocol> {
    let executor = executor.child();
    let h2 = tokio::time::timeout(TIMEOUT, async {
        let stream = TcpStream::connect((ip, port)).await?;
        request_http2(stream, name, "http", executor.clone()).await
    })
    .await;
    executor.shutdown().await;
    if matches!(h2, Ok(Ok(()))) {
        return Some(HttpProtocol::Http2);
    }
    let h1 = tokio::time::timeout(TIMEOUT, async {
        request(TcpStream::connect((ip, port)).await?, name).await
    })
    .await;
    if matches!(h1, Ok(Ok(()))) {
        return Some(HttpProtocol::Http1);
    }
    tracing::trace!(%ip, port, ?h2, ?h1, "container HTTP protocol probes failed");
    None
}

/// A response proves native HTTPS, including redirects and application errors.
pub(super) async fn responds_https(ip: Ipv4Addr, port: u16, name: &str) -> bool {
    let executor = Executor::default();
    let result = tokio::time::timeout(TIMEOUT, async {
        let stream = TcpStream::connect((ip, port)).await?;
        let stream = TlsConnector::from(configuration())
            .connect(ServerName::try_from(name.to_owned())?, stream)
            .await?;
        if stream.get_ref().1.alpn_protocol() == Some(b"h2") {
            request_http2(stream, name, "https", executor.clone()).await
        } else {
            request(stream, name).await
        }
    })
    .await;
    executor.shutdown().await;
    match result {
        Ok(Ok(())) => true,
        error => {
            tracing::trace!(%ip, port, ?error, "container HTTPS protocol probe failed");
            false
        }
    }
}

async fn request(stream: impl AsyncRead + AsyncWrite + Unpin, name: &str) -> Result<()> {
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    let request = Request::head("/")
        .header("Host", name)
        .header("User-Agent", "ArcBox/probe")
        .body(Empty::<Bytes>::new())?;
    // Drive the connection in this future so a timeout also closes the socket.
    let response = sender.send_request(request);
    tokio::pin!(response);
    tokio::select! {
        response = &mut response => response.map(|_| ()).context("HTTP probe response"),
        _ = connection => {
            // A parsed response still proves HTTP if TLS then closes without close_notify.
            response.await.map(|_| ()).context("HTTP probe response")
        },
    }
}

async fn request_http2(
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    name: &str,
    scheme: &str,
    executor: Executor,
) -> Result<()> {
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(executor, TokioIo::new(stream)).await?;
    let request = Request::head(format!("{scheme}://{name}/"))
        .header("User-Agent", "ArcBox/probe")
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request);
    tokio::pin!(response);
    tokio::select! {
        response = &mut response => response.map(|_| ()).context("HTTP/2 probe response"),
        _ = connection => response.await.map(|_| ()).context("HTTP/2 probe response"),
    }
}

pub(super) fn configuration() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let provider = rustls::crypto::ring::default_provider();
        let verifier = DiscoveryVerifier(provider.signature_verification_algorithms);
        let mut config = rustls::ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .expect("ring supports TLS")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(config)
    }))
}

/// Discovery accepts private certificates; clients still verify the native server.
#[derive(Debug)]
struct DiscoveryVerifier(rustls::crypto::WebPkiSupportedAlgorithms);

impl ServerCertVerifier for DiscoveryVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_util::rt::TokioExecutor;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn reject_http2(listener: &TcpListener) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut preface = [0; 24];
        stream.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    }

    async fn reply(mut stream: impl AsyncRead + AsyncWrite + Unpin, name: &str, response: &[u8]) {
        let mut request = vec![0; 1024];
        let len = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..len]).to_lowercase();
        assert!(request.starts_with("head / http/1.1\r\n"));
        assert!(request.contains(&format!("host: {name}\r\n")));
        assert!(request.contains("user-agent: arcbox/probe\r\n"));
        stream.write_all(response).await.unwrap();
    }

    #[tokio::test]
    async fn accepts_http_errors_but_rejects_other_protocols() {
        let name = "web.containers.test";
        for (response, expected) in [
            (
                &b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"[..],
                true,
            ),
            (&b"-ERR unknown command 'HEAD'\r\n"[..], false),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                reject_http2(&listener).await;
                reply(listener.accept().await.unwrap().0, name, response).await;
            });
            assert_eq!(
                Cache::default().http(Ipv4Addr::LOCALHOST, port, name).await,
                expected.then_some(HttpProtocol::Http1)
            );
            server.await.unwrap();
        }
    }

    fn tls_configuration() -> rustls::ServerConfig {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(
                    include_bytes!("testdata/probe-cert.der").to_vec(),
                )],
                PrivatePkcs8KeyDer::from(include_bytes!("testdata/probe-key.der").to_vec()).into(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn discovers_self_signed_https_with_the_domain_sni() {
        let name = "web.containers.test";
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(tls_configuration()))
                .accept(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.server_name(), Some(name));
            reply(tls, name, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        });
        assert!(responds_https(Ipv4Addr::LOCALHOST, port, name).await);
        server.await.unwrap();
    }

    async fn reply_http2(
        stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        scheme: &'static str,
    ) -> Result<(), hyper::Error> {
        let service =
            hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| async move {
                assert_eq!(request.method(), hyper::Method::HEAD);
                assert_eq!(request.uri().authority().unwrap(), "web.arcbox.local");
                assert_eq!(request.uri().scheme_str(), Some(scheme));
                assert_eq!(request.headers()["user-agent"], "ArcBox/probe");
                Ok::<_, std::convert::Infallible>(
                    hyper::Response::builder()
                        .status(401)
                        .body(Empty::<Bytes>::new())
                        .unwrap(),
                )
            });
        hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await
    }

    #[tokio::test]
    async fn discovers_http2_only_https_with_the_domain_authority() {
        let mut config = tls_configuration();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.server_name(), Some("web.arcbox.local"));
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
            reply_http2(tls, "https").await
        });
        let discovered = responds_https(Ipv4Addr::LOCALHOST, port, "web.arcbox.local").await;
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled(), "HTTP/2 fixture failed: {error}");
        }
        assert!(discovered);
    }

    #[tokio::test]
    async fn discovers_h2c_and_prefers_it_when_http1_also_responds() {
        for dual in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                if dual {
                    reply(
                        listener.accept().await.unwrap().0,
                        "web.arcbox.local",
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                }
                let (stream, _) = listener.accept().await.unwrap();
                reply_http2(stream, "http").await
            });
            if dual {
                request(
                    TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                        .await
                        .unwrap(),
                    "web.arcbox.local",
                )
                .await
                .unwrap();
            }
            assert_eq!(
                Cache::default()
                    .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                    .await,
                Some(HttpProtocol::Http2)
            );
            server.abort();
            if let Err(error) = server.await {
                assert!(error.is_cancelled(), "HTTP/2 fixture failed: {error}");
            }
        }
    }

    #[tokio::test]
    async fn bounded_probes_close_both_unresponsive_sockets() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = async {
            for h2 in [true, false] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).await.unwrap();
                assert!(request.starts_with(if h2 {
                    b"PRI * HTTP/2.0\r\n"
                } else {
                    b"HEAD / HTTP/1.1\r\n"
                }));
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            let mut cache = Cache::default();
            tokio::join!(
                cache.http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local"),
                server
            )
        })
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn outer_cancellation_joins_the_probe_socket_before_returning() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancelled = tokio_util::sync::CancellationToken::new();
        let stop = cancelled.clone();
        let probe = tokio::spawn(async move {
            super::super::http_tasks::run(&cancelled, async move |executor| {
                let _ = http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", executor).await;
                anyhow::bail!("protocol discovery completed before cancellation")
            })
            .await
        });
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut preface = [0; 24];
        peer.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), async {
            probe.await.unwrap().unwrap();
            peer.read_to_end(&mut Vec::new()).await.unwrap();
        })
        .await
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn replacing_a_listener_invalidates_its_cached_protocol() {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let netns = std::path::Path::new("/proc/self/ns/net");
        let original = super::super::listeners::read(std::process::id(), netns)
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            reject_http2(&listener).await;
            reply(
                listener.accept().await.unwrap().0,
                "web.arcbox.local",
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            )
            .await;
        });
        let mut cache = Cache::default();
        cache.observe(&original);
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            Some(HttpProtocol::Http1)
        );
        server.await.unwrap();
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .await
            .unwrap();
        let replacement = super::super::listeners::read(std::process::id(), netns)
            .await
            .unwrap();
        assert!(original.ports().contains(&port));
        assert!(replacement.ports().contains(&port));
        assert_ne!(original, replacement);
        cache.observe(&original);
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            Some(HttpProtocol::Http1)
        );
        cache.observe(&replacement);
        let server =
            tokio::spawn(
                async move { reply_http2(listener.accept().await.unwrap().0, "http").await },
            );
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            Some(HttpProtocol::Http2)
        );
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled(), "HTTP/2 fixture failed: {error}");
        }
    }

    #[tokio::test]
    async fn failed_probes_retry_and_verified_probes_wait_for_invalidation() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for response in [
                &b"-ERR starting\r\n"[..],
                &b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"[..],
            ] {
                reject_http2(&listener).await;
                reply(
                    listener.accept().await.unwrap().0,
                    "web.arcbox.local",
                    response,
                )
                .await;
            }
        });
        let mut cache = Cache::default();
        cache.observe(&super::super::listeners::Snapshot::default());
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            None
        );
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            Some(HttpProtocol::Http1)
        );
        server.await.unwrap();
        cache.observe(&super::super::listeners::Snapshot::default());
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            Some(HttpProtocol::Http1)
        );
        cache.invalidate();
        assert_eq!(
            cache
                .http(Ipv4Addr::LOCALHOST, port, "web.arcbox.local")
                .await,
            None
        );
    }
}
