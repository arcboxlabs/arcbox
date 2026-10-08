//! TLS termination for currently owned, concrete `.local` names.
//!
//! Port 443 of each container with an HTTP port is REDIRECTed (nat
//! PREROUTING) to [`PROXY_PORT`] in the VM's own namespace. The proxy reads
//! the address the client dialled from conntrack (`SO_ORIGINAL_DST`),
//! presents a certificate the local CA signs for the SNI name, and forwards
//! HTTP with `X-Forwarded-Proto: https` to the container. Traffic from the Mac never
//! passes through the daemon, so the guest is the only place to terminate.
//!
//! [`PROXY_PORT`] is taken in the VM's namespace, so a container cannot
//! publish that host port. It sits above the kernel's ephemeral range,
//! which dockerd picks unnamed publishes from, so only an explicit
//! `-p 61443:…` collides.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{LazyConfigAcceptor, rustls};
use tokio_util::sync::CancellationToken;

use super::WebServices;
use super::certificates::Certificates;
use super::http_port::HTTPS_PORT;

#[cfg(test)]
#[path = "https_tests.rs"]
mod tests;

/// Where the 443 rules redirect to, in the VM's own namespace.
pub(super) const PROXY_PORT: u16 = 61443;

/// A handshake or upstream connect that takes longer is given up.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed accept before the next one.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The listening proxy.
pub(super) struct Https {
    listener: TcpListener,
    certificates: Arc<Certificates>,
}

impl Https {
    /// Binds the host signing channel and [`PROXY_PORT`].
    pub(super) async fn bind() -> Result<Self> {
        let certificates = Arc::new(Certificates::bind()?);
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PROXY_PORT))
            .await
            .with_context(|| format!("binding port {PROXY_PORT}"))?;
        Ok(Self {
            listener,
            certificates,
        })
    }

    /// Serves connections until `cancel`, one task each.
    pub(super) async fn serve(self, services: WebServices, cancel: CancellationToken) {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (client, peer) = tokio::select! {
                () = cancel.cancelled() => break,
                Some(result) = connections.join_next() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "container domain HTTPS task failed");
                    }
                    continue;
                }
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
            let certificates = Arc::clone(&self.certificates);
            let services = services.clone();
            let cancel = cancel.clone();
            connections.spawn(async move {
                let result = tokio::select! {
                    () = cancel.cancelled() => return,
                    result = relay(client, &certificates, &services) => result,
                };
                if let Err(e) = result {
                    tracing::debug!(%peer, error = %format!("{e:#}"), "container domain HTTPS connection ended");
                }
            });
        }
        connections.shutdown().await;
    }
}

/// Terminates one client's TLS and relays it to the container it dialled.
async fn relay(
    client: TcpStream,
    certificates: &Certificates,
    services: &WebServices,
) -> Result<()> {
    let dialled = original_destination(&client).context("reading the dialled address")?;
    // Anything but a redirected container 443 reached the port directly.
    ensure!(
        dialled.port() == HTTPS_PORT,
        "{dialled} is no container's HTTPS port"
    );
    relay_to(
        client,
        *dialled.ip(),
        services,
        async |name| certificates.configuration(name).await,
        async |address| TcpStream::connect(address).await,
    )
    .await
}

async fn relay_to(
    client: TcpStream,
    container: Ipv4Addr,
    services: &WebServices,
    configuration: impl AsyncFnOnce(&str) -> Result<Arc<rustls::ServerConfig>>,
    connect: impl AsyncFnOnce((Ipv4Addr, u16)) -> io::Result<TcpStream>,
) -> Result<()> {
    let source = client
        .peer_addr()
        .context("reading the TLS client address")?
        .ip();
    let hello = tokio::time::timeout(
        TIMEOUT,
        LazyConfigAcceptor::new(rustls::server::Acceptor::default(), client),
    )
    .await
    .context("TLS ClientHello timed out")??;
    let name = hello
        .client_hello()
        .server_name()
        .context("TLS client omitted SNI")?
        .to_owned();
    let route = services
        .observe_managed_tls(&name, container, source)
        .await?;
    let tls = tokio::time::timeout(TIMEOUT, async {
        let config = configuration(&name).await?;
        services.verify_tls_route(&name, container, &route).await?;
        Ok::<_, anyhow::Error>(hello.into_stream(config).await?)
    })
    .await
    .context("TLS handshake timed out")?
    .context("TLS handshake")?;
    services.verify_tls_route(&name, container, &route).await?;
    let http_port = route.http_port;
    let upstream = tokio::time::timeout(TIMEOUT, connect((container, http_port)))
        .await
        .context("connecting to the container timed out")
        .and_then(|result| {
            result.with_context(|| format!("connecting to {container}:{http_port}"))
        });
    services.verify_tls_route(&name, container, &route).await?;
    let upstream = match upstream {
        Ok(upstream) => upstream,
        Err(error) => {
            services
                .invalidate_tls_route(&name, container, &route)
                .await;
            return Err(error);
        }
    };
    super::http_relay::serve(tls, upstream).await
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
