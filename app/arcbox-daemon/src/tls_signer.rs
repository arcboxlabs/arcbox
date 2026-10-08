//! One parked host connection serves one guest CSR, then reconnects.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arcbox_constants::ports::TLS_SIGNING_PORT;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime};
use arcbox_local_ca::{KeychainAuthority, MAX_CERTIFICATE_SIZE};
use arcbox_transport::vsock::{VsockShutdown, VsockStream};
use futures::{SinkExt, StreamExt};
use tokio_util::{codec::LengthDelimitedCodec, sync::CancellationToken};

use crate::context::DaemonContext;

pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let Some(authority) = ctx.local_ca.get().cloned() else {
        // Preparation already published the HTTPS error without blocking Docker.
        return;
    };
    let runtime = Arc::clone(runtime);
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(run(runtime, authority, shutdown));
}

async fn run(
    runtime: Arc<Runtime>,
    authority: Arc<KeychainAuthority>,
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

async fn serve_once(runtime: &Arc<Runtime>, authority: &Arc<KeychainAuthority>) -> Result<()> {
    let runtime = Arc::clone(runtime);
    let socket = tokio::task::spawn_blocking(move || {
        let fd = runtime.connect_vsock_port(DEFAULT_MACHINE_NAME, TLS_SIGNING_PORT)?;
        // SAFETY: the hypervisor returns a newly opened descriptor and transfers ownership.
        Ok::<_, arcbox_core::CoreError>(unsafe { OwnedFd::from_raw_fd(fd) })
    })
    .await??;
    let stream = VsockStream::from_fd_with_shutdown(socket, VsockShutdown::CloseOnDropOnly)?;
    let mut frames = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_CERTIFICATE_SIZE)
        .new_framed(stream);
    // VZ requires the host to keep reading while the connection is parked.
    let request = frames
        .next()
        .await
        .context("guest closed the signing connection")??;
    let authority = Arc::clone(authority);
    let certificate = tokio::task::spawn_blocking(move || authority.sign(&request)).await??;
    tokio::time::timeout(Duration::from_secs(10), frames.send(certificate.into()))
        .await
        .context("guest did not read its certificate")??;
    Ok(())
}
