//! TLS termination for `https://<name>.arcbox.local`.
//!
//! Port 443 of each container with an HTTP port is REDIRECTed (nat
//! PREROUTING) to [`PROXY_PORT`] in the VM's own namespace. The proxy reads
//! the address the client dialled from conntrack (`SO_ORIGINAL_DST`),
//! presents a certificate the local CA signs for the SNI name, and relays
//! the plaintext to that container's HTTP port. Traffic from the Mac never
//! passes through the daemon, so the guest is the only place to terminate.
//!
//! [`PROXY_PORT`] is taken in the VM's namespace, so a container cannot
//! publish that host port. It sits above the kernel's ephemeral range,
//! which dockerd picks unnamed publishes from, so only an explicit
//! `-p 61443:…` collides.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_constants::paths::guest;
use arcbox_local_ca::LeafMinter;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::HttpPorts;
use super::http_port::HTTPS_PORT;

/// Where the 443 rules redirect to, in the VM's own namespace.
pub(super) const PROXY_PORT: u16 = 61443;

/// A handshake or upstream connect that takes longer is given up.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed accept before the next one.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The listening proxy.
pub(super) struct Https {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl Https {
    /// Loads the CA the daemon put on the share and binds [`PROXY_PORT`].
    pub(super) async fn bind() -> Result<Self> {
        let dir = Path::new(guest::MOUNT).join(guest::TLS);
        let minter = LeafMinter::load(&dir)
            .with_context(|| format!("loading the local CA from {}", dir.display()))?;
        let mut config = minter.into_server_config()?;
        // The plaintext reaches the container as HTTP/1.1; h2 would not.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PROXY_PORT))
            .await
            .with_context(|| format!("binding port {PROXY_PORT}"))?;
        Ok(Self {
            listener,
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// Serves connections until `cancel`, one task each.
    pub(super) async fn serve(self, ports: HttpPorts, cancel: CancellationToken) {
        loop {
            let (client, peer) = tokio::select! {
                () = cancel.cancelled() => return,
                accepted = self.listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        // Out of descriptors, most likely: retrying at once
                        // would only spin.
                        tracing::warn!(error = %e, "container domain HTTPS accept failed");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                        continue;
                    }
                },
            };
            let acceptor = self.acceptor.clone();
            let ports = ports.clone();
            tokio::spawn(async move {
                if let Err(e) = relay(client, &acceptor, &ports).await {
                    tracing::debug!(%peer, error = %format!("{e:#}"), "container domain HTTPS connection ended");
                }
            });
        }
    }
}

/// Terminates one client's TLS and relays it to the container it dialled.
async fn relay(client: TcpStream, acceptor: &TlsAcceptor, ports: &HttpPorts) -> Result<()> {
    let dialled = original_destination(&client).context("reading the dialled address")?;
    // Anything but a redirected container 443 reached the port directly.
    ensure!(
        dialled.port() == HTTPS_PORT,
        "{dialled} is no container's HTTPS port"
    );
    let container = *dialled.ip();
    let http_port = ports
        .get(container)
        .with_context(|| format!("{container} serves no HTTP port"))?;
    let mut tls = tokio::time::timeout(TIMEOUT, acceptor.accept(client))
        .await
        .context("TLS handshake timed out")?
        .context("TLS handshake")?;
    let mut upstream = tokio::time::timeout(TIMEOUT, TcpStream::connect((container, http_port)))
        .await
        .context("connecting to the container timed out")?
        .with_context(|| format!("connecting to {container}:{http_port}"))?;
    tokio::io::copy_bidirectional(&mut tls, &mut upstream).await?;
    Ok(())
}

/// The destination the client dialled before a nat rule redirected it.
#[cfg(target_os = "linux")]
fn original_destination(stream: &TcpStream) -> io::Result<SocketAddrV4> {
    use nix::sys::socket::{getsockopt, sockopt::OriginalDst};
    let addr = getsockopt(stream, OriginalDst)?;
    Ok(SocketAddrV4::new(
        Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
        u16::from_be(addr.sin_port),
    ))
}

/// Conntrack exists only in the Linux guest.
#[cfg(not(target_os = "linux"))]
fn original_destination(_: &TcpStream) -> io::Result<SocketAddrV4> {
    Err(io::ErrorKind::Unsupported.into())
}
