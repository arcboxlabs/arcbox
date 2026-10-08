//! One parked host connection signs one guest CSR with the current published root.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_constants::ports::TLS_SIGNING_PORT;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime};
use arcbox_local_ca::{KeychainAuthority, MAX_CERTIFICATE_SIZE};
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
    let runtime = Arc::clone(runtime);
    let socket = tokio::task::spawn_blocking(move || {
        let fd = runtime.connect_vsock_port(DEFAULT_MACHINE_NAME, TLS_SIGNING_PORT)?;
        // SAFETY: the hypervisor returns a newly opened descriptor and transfers ownership.
        Ok::<_, arcbox_core::CoreError>(unsafe { OwnedFd::from_raw_fd(fd) })
    })
    .await??;
    let stream = VsockStream::from_fd_with_shutdown(socket, VsockShutdown::CloseOnDropOnly)?;
    serve_request(stream, authority, |authority, request| {
        Ok(authority.sign(request)?)
    })
    .await
}

async fn serve_request<A, S>(
    stream: S,
    current: &watch::Receiver<Option<Arc<A>>>,
    sign: impl FnOnce(&A, &[u8]) -> Result<Vec<u8>> + Send + 'static,
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
    let authority = current
        .borrow()
        .clone()
        .context("local CA is not prepared")?;
    let signing_authority = Arc::clone(&authority);
    let certificate =
        tokio::task::spawn_blocking(move || sign(&signing_authority, &request)).await??;
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

    #[tokio::test]
    async fn parked_connection_selects_authority_when_the_request_arrives() {
        let (sender, current) = watch::channel(None::<Arc<u8>>);
        let (host, guest) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            serve_request(host, &current, |authority, request| {
                Ok(vec![*authority, request[0]])
            })
            .await
        });
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(1)));
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(2)));
        let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
        guest.send(vec![7].into()).await.unwrap();
        assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[2, 7]);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn changed_authority_discards_the_in_flight_signature() {
        let original = Arc::new(1_u8);
        for replacement in [None, Some(Arc::new(2)), Some(Arc::clone(&original))] {
            let unchanged = replacement
                .as_ref()
                .is_some_and(|authority| Arc::ptr_eq(authority, &original));
            let (sender, current) = watch::channel(Some(Arc::clone(&original)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let task = tokio::spawn(async move {
                serve_request(host, &current, move |authority, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![*authority])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            guest.send(vec![7].into()).await.unwrap();
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
