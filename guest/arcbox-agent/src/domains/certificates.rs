//! Leaf keys stay here; the host returns certificates over a parked vsock.

mod cache;

use std::{path::PathBuf, sync::Arc, time::SystemTime};

use anyhow::{Context, Result, ensure};
use arcbox_constants::paths::guest::{MOUNT, TLS, TLS_CA_CERT};
use arcbox_local_ca::{CertificateRequest, CertificateValidity, PublicAuthority};
use tokio::sync::watch;
use tokio_rustls::rustls::ServerConfig;

use self::cache::{Cache, Cached};

pub(super) struct Certificates {
    #[cfg(target_os = "linux")]
    listener: tokio::sync::Mutex<tokio_vsock::VsockListener>,
    cache: Cache,
    root: RootSource,
}

pub(super) enum RootSource {
    PublishedFile(PathBuf),
    Session(watch::Receiver<Option<Arc<PublicAuthority>>>),
}

impl RootSource {
    pub(super) fn system() -> Self {
        Self::PublishedFile(PathBuf::from(MOUNT).join(TLS).join(TLS_CA_CERT))
    }

    async fn authority(&self) -> Result<Arc<PublicAuthority>> {
        let authority = match self {
            Self::PublishedFile(path) => {
                let pem = tokio::fs::read(path).await.with_context(|| {
                    format!("read published local HTTPS root {}", path.display())
                })?;
                Arc::new(
                    PublicAuthority::from_pem(&pem)
                        .with_context(|| format!("invalid local HTTPS root {}", path.display()))?,
                )
            }
            Self::Session(root) => root
                .borrow()
                .clone()
                .context("machine HTTPS root is unavailable")?,
        };
        ensure!(
            authority.validity().is_valid_at(SystemTime::now().into()),
            "local HTTPS root is not currently valid"
        );
        Ok(authority)
    }
}

impl Certificates {
    pub(super) fn bind(root: RootSource) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let listener = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(
            tokio_vsock::VMADDR_CID_ANY,
            arcbox_constants::ports::TLS_SIGNING_PORT,
        ))?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            listener: tokio::sync::Mutex::new(listener),
            cache: Cache::default(),
            root,
        })
    }

    pub(super) async fn configuration(&self, name: &str) -> Result<Arc<ServerConfig>> {
        let name = name.to_ascii_lowercase();
        self.cache
            .configuration(&name, self.issue(&name), || async {
                Ok(self.root.authority().await?.der().to_vec())
            })
            .await
    }

    async fn issue(&self, name: &str) -> Result<Cached> {
        let request = CertificateRequest::new(name)?;
        let certificate = self.sign(request.der()).await?;
        let authority = self.root.authority().await?;
        let validity = CertificateValidity::from_der(&certificate)?;
        let mut configuration = request.complete(certificate, &authority)?;
        configuration.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Cached::new(
            Arc::new(configuration),
            validity,
            SystemTime::now(),
            authority.der().to_vec(),
        )
    }

    #[cfg(target_os = "linux")]
    async fn sign(&self, request: &[u8]) -> Result<Vec<u8>> {
        use futures::{SinkExt, StreamExt};
        use tokio_util::codec::LengthDelimitedCodec;
        loop {
            let (stream, peer) = self.listener.lock().await.accept().await?;
            if peer.cid() != tokio_vsock::VMADDR_CID_HOST {
                tracing::warn!(cid = peer.cid(), "rejected non-host CA signing connection");
                continue;
            }
            let mut frames = LengthDelimitedCodec::builder()
                .max_frame_length(arcbox_local_ca::MAX_CERTIFICATE_SIZE)
                .new_framed(stream);
            frames.send(request.to_vec().into()).await?;
            return Ok(frames
                .next()
                .await
                .context("host closed the CA signing connection")??
                .to_vec());
        }
    }

    #[cfg(not(target_os = "linux"))]
    async fn sign(&self, _: &[u8]) -> Result<Vec<u8>> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported)).context("vsock requires Linux")
    }
}

/// Primes the real cache and rejects any subsequent signing attempt.
#[cfg(all(test, target_os = "linux"))]
pub(super) struct CachedConfiguration(Cache);

#[cfg(all(test, target_os = "linux"))]
impl CachedConfiguration {
    pub(super) async fn new(name: &str, config: Arc<ServerConfig>) -> Self {
        let cache = Cache::default();
        let now = SystemTime::now();
        cache
            .configuration(
                name,
                async {
                    Cached::new(
                        config,
                        CertificateValidity {
                            not_before: (now - std::time::Duration::from_secs(60)).into(),
                            not_after: (now + std::time::Duration::from_secs(3600)).into(),
                        },
                        now,
                        b"test-root".to_vec(),
                    )
                },
                || async { Ok(b"test-root".to_vec()) },
            )
            .await
            .unwrap();
        Self(cache)
    }

    pub(super) async fn configuration(&self, name: &str) -> Result<Arc<ServerConfig>> {
        self.0
            .configuration(
                name,
                async { panic!("cached TLS must not request another signature") },
                || async { Ok(b"test-root".to_vec()) },
            )
            .await
    }
}
