//! Guest-side vsock proxies that bridge host traffic to local Unix / TCP
//! sockets:
//!
//! - **Docker API**: vsock listener → `/var/run/docker.sock` (Unix). The
//!   channel is framed with [`HalfCloseStream`] so each direction can close
//!   on its own: the vsock fd cannot half-close on macOS, and without an
//!   in-band EOF a `docker run -i` never delivered stdin EOF to its
//!   container (arcboxlabs/arcbox#268). The host proxy speaks the same
//!   framing (agent protocol v4).
//! - **Kubernetes API**: vsock listener → `127.0.0.1:KUBERNETES_API_GUEST_PORT`
//!   (TCP, k3s API server bound to localhost). Raw bytes; TLS closes itself.

use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::{Context, Result};
use arcbox_transport::vsock::HalfCloseStream;
use tokio::net::{TcpStream, UnixStream};
use tokio_vsock::VsockStream;

use arcbox_constants::paths::DOCKER_API_UNIX_SOCKET;
use arcbox_constants::ports::KUBERNETES_API_GUEST_PORT;

use super::cmdline::{docker_api_vsock_port, kubernetes_api_vsock_port};
use super::vsock::bind_vsock_listener_with_retry;

pub(super) async fn run_docker_api_proxy() -> Result<()> {
    let port = docker_api_vsock_port();
    let mut listener = bind_vsock_listener_with_retry(port, "docker api proxy").await?;
    tracing::info!("Docker API proxy listening on vsock port {}", port);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                tracing::info!("Docker API proxy accepted connection from {:?}", peer_addr);
                tokio::spawn(async move {
                    if let Err(e) = proxy_docker_api_connection(stream).await {
                        tracing::warn!("Docker API proxy connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Docker API proxy accept failed: {}", e);
            }
        }
    }
}

async fn proxy_docker_api_connection(vsock_stream: VsockStream) -> Result<()> {
    let mut unix_stream = UnixStream::connect(DOCKER_API_UNIX_SOCKET)
        .await
        .context("failed to connect guest docker unix socket")?;
    tracing::info!("Docker proxy: connected to {}", DOCKER_API_UNIX_SOCKET);

    // `copy_bidirectional` shuts down the write half of one side when the
    // other side's reader hits EOF. Towards dockerd that is a real
    // `SHUT_WR` on the Unix socket, which is how a container learns its
    // stdin is done; towards the host it is the framed EOF marker, since
    // the vsock fd would swallow a plain shutdown.
    let mut host = HalfCloseStream::new(vsock_stream);
    let result = tokio::io::copy_bidirectional(&mut host, &mut unix_stream).await;
    match result {
        Ok((host_to_docker, docker_to_host)) => {
            tracing::info!(
                "Docker proxy session done: vsock→unix={} bytes, unix→vsock={} bytes",
                host_to_docker,
                docker_to_host,
            );
            Ok(())
        }
        Err(e) if is_peer_closed_io_error(&e) => {
            tracing::debug!("Docker proxy session ended by peer: {}", e);
            Ok(())
        }
        Err(e) => Err(e).context("docker api proxy copy failed"),
    }
}

/// A peer dropping its end mid-session is routine teardown (the CLI was
/// killed, dockerd restarted), not a proxy fault.
fn is_peer_closed_io_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof
    )
}

pub(super) async fn run_kubernetes_api_proxy() -> Result<()> {
    let port = kubernetes_api_vsock_port();
    let mut listener = bind_vsock_listener_with_retry(port, "kubernetes api proxy").await?;
    tracing::info!("Kubernetes API proxy listening on vsock port {}", port);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                tracing::debug!(
                    "Kubernetes API proxy accepted connection from {:?}",
                    peer_addr
                );
                tokio::spawn(async move {
                    if let Err(e) = proxy_kubernetes_api_connection(stream).await {
                        tracing::debug!("Kubernetes API proxy connection ended: {}", e);
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Kubernetes API proxy accept failed: {}", e);
            }
        }
    }
}

async fn proxy_kubernetes_api_connection(mut vsock_stream: VsockStream) -> Result<()> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, KUBERNETES_API_GUEST_PORT);
    let mut tcp_stream = TcpStream::connect(addr)
        .await
        .context("failed to connect guest kubernetes api socket")?;

    let _ = tokio::io::copy_bidirectional(&mut vsock_stream, &mut tcp_stream)
        .await
        .context("kubernetes api proxy copy failed")?;
    Ok(())
}
