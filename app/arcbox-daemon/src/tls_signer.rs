//! One parked host connection signs one verified, host-owned CSR with the current published root.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_constants::ports::TLS_SIGNING_PORT;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime};
use arcbox_local_ca::{KeychainAuthority, MAX_CERTIFICATE_SIZE, VerifiedCertificateRequest};
use arcbox_transport::vsock::{VsockShutdown, VsockStream};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::watch,
};
use tokio_util::{codec::LengthDelimitedCodec, sync::CancellationToken};

use crate::context::DaemonContext;

pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let authority = ctx.local_ca.subscribe();
    let runtime = Arc::clone(runtime);
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(run(runtime, authority, shutdown));
}

async fn run(
    runtime: Arc<Runtime>,
    authority: watch::Receiver<Option<Arc<KeychainAuthority>>>,
    shutdown: CancellationToken,
) {
    let mut state = runtime.subscribe_system_vm_state();
    loop {
        let ready = state.wait_for(|state| state.is_ready());
        tokio::select! {
            () = shutdown.cancelled() => return,
            ready = ready => if ready.is_err() { return; },
        }
        let result = tokio::select! {
            () = shutdown.cancelled() => return,
            result = serve_once(&runtime, &authority) => result,
        };
        if let Err(error) = result {
            tracing::warn!(%error, "local CA signing connection failed");
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

async fn serve_once(
    runtime: &Arc<Runtime>,
    authority: &watch::Receiver<Option<Arc<KeychainAuthority>>>,
) -> Result<()> {
    let connecting_runtime = Arc::clone(runtime);
    let socket = tokio::task::spawn_blocking(move || {
        let fd = connecting_runtime.connect_vsock_port(DEFAULT_MACHINE_NAME, TLS_SIGNING_PORT)?;
        // SAFETY: the hypervisor returns a newly opened descriptor and transfers ownership.
        Ok::<_, arcbox_core::CoreError>(unsafe { OwnedFd::from_raw_fd(fd) })
    })
    .await??;
    let stream = VsockStream::from_fd_with_shutdown(socket, VsockShutdown::CloseOnDropOnly)?;
    serve_request(stream, authority, runtime, |authority, request| {
        Ok(authority.sign(request)?)
    })
    .await
}

async fn serve_request<A, S>(
    stream: S,
    current: &watch::Receiver<Option<Arc<A>>>,
    runtime: &Runtime,
    sign: impl FnOnce(&A, &VerifiedCertificateRequest) -> Result<Vec<u8>> + Send + 'static,
) -> Result<()>
where
    A: Send + Sync + 'static,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut frames = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_CERTIFICATE_SIZE)
        .new_framed(stream);
    // VZ requires the host to keep reading while the connection is parked.
    let request = frames
        .next()
        .await
        .context("guest closed the signing connection")??;
    let request = VerifiedCertificateRequest::try_from(request.as_ref())?;
    let name = request.dns_name().to_owned();
    let revision = runtime
        .https_signing_revision(&name)
        .await
        .context("CSR DNS name is not owned by a current host container")?;
    let authority = current
        .borrow()
        .clone()
        .context("local CA is not prepared")?;
    let signing_authority = Arc::clone(&authority);
    let certificate =
        tokio::task::spawn_blocking(move || sign(&signing_authority, &request)).await??;
    ensure!(
        runtime.https_signing_revision(&name).await == Some(revision),
        "DNS ownership changed during signing"
    );
    ensure!(
        current
            .borrow()
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &authority)),
        "local CA changed during signing"
    );
    tokio::time::timeout(Duration::from_secs(10), frames.send(certificate.into()))
        .await
        .context("guest did not read its certificate")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_local_ca::CertificateRequest;

    async fn runtime() -> (Arc<Runtime>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            Runtime::new(arcbox_core::Config {
                data_dir: directory.path().to_owned(),
                ..Default::default()
            })
            .unwrap(),
        );
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await;
        (runtime, directory)
    }

    #[tokio::test]
    async fn unowned_or_tampered_requests_never_reach_the_signer() {
        let (runtime, _directory) = runtime().await;
        let unknown = CertificateRequest::new("unknown.arcbox.local").unwrap();
        let pending = CertificateRequest::new("web.arcbox.local").unwrap();
        let mut tampered = pending.der().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        for request in [unknown.der().to_vec(), tampered] {
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (host, guest) = tokio::io::duplex(1024);
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            guest.send(request.into()).await.unwrap();
            let (signed, signature) = tokio::sync::oneshot::channel();
            let result = serve_request(host, &current, &runtime, move |_, _| {
                signed.send(()).unwrap();
                Ok(vec![1])
            })
            .await;
            assert!(result.is_err());
            assert!(signature.await.is_err());
            assert!(guest.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn changed_dns_ownership_discards_the_in_flight_signature() {
        for replacement in [None, Some("replacement"), Some("container")] {
            let (runtime, _directory) = runtime().await;
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let signing_runtime = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                serve_request(host, &current, &signing_runtime, move |authority, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![*authority])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            guest.send(request.der().to_vec().into()).await.unwrap();
            signing.await.unwrap();
            if let Some(owner) = replacement {
                runtime
                    .register_dns(owner, &["web".into()], "10.0.2.3".parse().unwrap())
                    .await;
            } else {
                runtime.deregister_dns_by_id("container").await;
            }
            finish.send(()).unwrap();
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("DNS ownership changed during signing")
            );
            assert!(guest.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn parked_connection_selects_authority_when_the_request_arrives() {
        let (runtime, _directory) = runtime().await;
        let (sender, current) = watch::channel(None::<Arc<u8>>);
        let (host, guest) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            serve_request(host, &current, &runtime, |authority, request| {
                assert_eq!(request.dns_name(), "web.arcbox.local");
                Ok(vec![*authority])
            })
            .await
        });
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(1)));
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(2)));
        let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
        let request = CertificateRequest::new("web.arcbox.local").unwrap();
        guest.send(request.der().to_vec().into()).await.unwrap();
        assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[2]);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn changed_authority_discards_the_in_flight_signature() {
        let (runtime, _directory) = runtime().await;
        let original = Arc::new(1_u8);
        for replacement in [None, Some(Arc::new(2)), Some(Arc::clone(&original))] {
            let unchanged = replacement
                .as_ref()
                .is_some_and(|authority| Arc::ptr_eq(authority, &original));
            let (sender, current) = watch::channel(Some(Arc::clone(&original)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let runtime = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                serve_request(host, &current, &runtime, move |authority, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![*authority])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            guest.send(request.der().to_vec().into()).await.unwrap();
            signing.await.unwrap();
            sender.send_replace(replacement);
            finish.send(()).unwrap();
            let result = task.await.unwrap();
            if unchanged {
                result.unwrap();
                assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[1]);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("changed during signing")
                );
                assert!(guest.next().await.is_none());
            }
        }
    }
}
