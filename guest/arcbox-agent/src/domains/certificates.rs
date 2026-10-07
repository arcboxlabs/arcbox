//! Leaf keys stay here; the host returns certificates over a parked vsock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use arcbox_local_ca::CertificateRequest;
use tokio_rustls::rustls::ServerConfig;

const RENEW_AFTER: Duration = Duration::from_secs(23 * 24 * 60 * 60);
const MAX_CACHED: usize = 256;

pub(super) struct Certificates {
    #[cfg(target_os = "linux")]
    listener: tokio::sync::Mutex<tokio_vsock::VsockListener>,
    cache: Mutex<HashMap<String, (SystemTime, Arc<ServerConfig>)>>,
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
            cache: Mutex::default(),
        })
    }

    pub(super) async fn configuration(&self, name: &str) -> Result<Arc<ServerConfig>> {
        let name = name.to_ascii_lowercase();
        let now = SystemTime::now();
        if let Some((created, config)) = self
            .cache
            .lock()
            .expect("certificate cache poisoned")
            .get(&name)
        {
            if now
                .duration_since(*created)
                .is_ok_and(|age| age < RENEW_AFTER)
            {
                return Ok(Arc::clone(config));
            }
        }
        let request = CertificateRequest::new(&name)?;
        let certificate = self.sign(request.der()).await?;
        let config = Arc::new(request.complete(certificate)?);
        let mut cache = self.cache.lock().expect("certificate cache poisoned");
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(name, (now, Arc::clone(&config)));
        Ok(config)
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
