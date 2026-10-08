//! Protocol discovery against the container address, outside PREROUTING NAT.

use std::collections::BTreeSet;
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

const TIMEOUT: Duration = Duration::from_secs(1);

/// Cache only successful probes until socket identities or container facts change.
#[derive(Default)]
pub(super) struct Cache {
    listeners: Option<super::listeners::Snapshot>,
    verified: BTreeSet<(u16, bool)>,
}

impl Cache {
    pub(super) fn observe(&mut self, listeners: &super::listeners::Snapshot) {
        if self.listeners.as_ref() != Some(listeners) {
            self.invalidate();
            self.listeners = Some(listeners.clone());
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.verified.clear();
    }

    pub(super) async fn responds(
        &mut self,
        ip: Ipv4Addr,
        port: u16,
        name: &str,
        tls: bool,
    ) -> bool {
        if self.verified.contains(&(port, tls)) {
            return true;
        }
        if responds(ip, port, name, tls).await {
            self.verified.insert((port, tls));
            return true;
        }
        false
    }
}

/// A response proves the protocol, including redirects and application errors.
pub(super) async fn responds(ip: Ipv4Addr, port: u16, name: &str, tls: bool) -> bool {
    let result = tokio::time::timeout(TIMEOUT, async {
        let stream = TcpStream::connect((ip, port)).await?;
        if tls {
            let stream = TlsConnector::from(configuration())
                .connect(ServerName::try_from(name.to_owned())?, stream)
                .await?;
            request(stream, name).await
        } else {
            request(stream, name).await
        }
    })
    .await;
    match result {
        Ok(Ok(())) => true,
        error => {
            tracing::trace!(%ip, port, tls, ?error, "container protocol probe failed");
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

fn configuration() -> Arc<rustls::ClientConfig> {
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
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn reply(mut stream: impl AsyncRead + AsyncWrite + Unpin, response: &[u8]) {
        let mut request = vec![0; 1024];
        let len = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..len]).to_lowercase();
        assert!(request.starts_with("head / http/1.1\r\n"));
        assert!(request.contains("host: web.arcbox.local\r\n"));
        assert!(request.contains("user-agent: arcbox/probe\r\n"));
        stream.write_all(response).await.unwrap();
    }

    #[tokio::test]
    async fn accepts_http_errors_but_rejects_other_protocols() {
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
                reply(listener.accept().await.unwrap().0, response).await;
            });
            assert_eq!(
                responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", false).await,
                expected
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn discovers_self_signed_https_with_the_domain_sni() {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        let config = rustls::ServerConfig::builder_with_provider(
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
        .unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.server_name(), Some("web.arcbox.local"));
            reply(tls, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        });
        assert!(responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", true).await);
        server.await.unwrap();
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
                reply(listener.accept().await.unwrap().0, response).await;
            }
        });
        let mut cache = Cache::default();
        cache.observe(&super::super::listeners::Snapshot::default());
        assert!(
            !cache
                .responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", false)
                .await
        );
        assert!(
            cache
                .responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", false)
                .await
        );
        server.await.unwrap();
        cache.observe(&super::super::listeners::Snapshot::default());
        assert!(
            cache
                .responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", false)
                .await
        );
        cache.invalidate();
        assert!(
            !cache
                .responds(Ipv4Addr::LOCALHOST, port, "web.arcbox.local", false)
                .await
        );
    }
}
