//! Leaf keys stay here; the host returns certificates over a parked vsock.

mod cache;

use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use arcbox_local_ca::{CertificateRequest, CertificateValidity};
use tokio_rustls::rustls::ServerConfig;

use self::cache::{Cache, Cached};

pub(super) struct Certificates {
    #[cfg(target_os = "linux")]
    listener: tokio::sync::Mutex<tokio_vsock::VsockListener>,
    cache: Cache,
}

impl Certificates {
    pub(super) fn bind() -> Result<Self> {
        #[cfg(target_os = "linux")]
        let listener = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(
            tokio_vsock::VMADDR_CID_ANY,
            arcbox_constants::ports::TLS_SIGNING_PORT,
        ))?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            listener: tokio::sync::Mutex::new(listener),
            cache: Cache::default(),
        })
    }

    pub(super) async fn configuration(&self, name: &str) -> Result<Arc<ServerConfig>> {
        let name = name.to_ascii_lowercase();
        self.cache.configuration(&name, self.issue(&name)).await
    }

    async fn issue(&self, name: &str) -> Result<Cached> {
        let request = CertificateRequest::new(name)?;
        let certificate = self.sign(request.der()).await?;
        let validity = CertificateValidity::from_der(&certificate)?;
        Cached::new(
            Arc::new(request.complete(certificate)?),
            validity,
            SystemTime::now(),
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
