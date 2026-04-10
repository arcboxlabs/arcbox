//! Host-side TCP port forwarder over vsock.
//!
//! [`VsockPortForwarder`] manages TCP listeners on host ports. For each
//! accepted connection it opens a vsock pipe to the guest port forwarding
//! service, performs the 6-byte handshake, and relays data bidirectionally.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use arcbox_constants::ports::PORT_FORWARD_VSOCK_PORT;

use crate::protocol;

/// Error type for port forwarding operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("vsock connect failed: {0}")]
    VsockConnect(String),
    #[error("handshake failed: status {status:#04x} ({desc})")]
    Handshake { status: u8, desc: &'static str },
    #[error("rule already exists for {0}")]
    DuplicateRule(SocketAddr),
}

/// Platform-specific vsock connection factory.
///
/// Implementations provide an fd connected to the guest's vsock port
/// forwarding service. The fd must support standard read/write I/O.
///
/// - **HV backend**: creates a socketpair via `VsockConnectionManager`
/// - **VZ backend**: calls `VZVirtioSocketDevice.connect()`
/// - **Linux**: opens an `AF_VSOCK` socket
pub trait VsockConnector: Send + Sync + 'static {
    /// Opens a vsock connection to the given guest port.
    /// Returns `(fd, host_port)` where fd is a raw fd the caller owns
    /// and host_port is the ephemeral vsock port assigned to this connection.
    fn connect(&self, guest_port: u32) -> Result<(RawFd, u32), String>;

    /// Promotes a host TCP stream to the inline inject path, bypassing
    /// the socketpair for host→guest data. Returns true if promotion
    /// succeeded. Default: no inline support (falls back to socketpair relay).
    fn promote_inline(
        &self,
        _stream: std::net::TcpStream,
        _host_port: u32,
        _guest_port: u32,
    ) -> bool {
        false
    }
}

/// A single port forwarding rule.
struct ActiveRule {
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

/// Host-side TCP port forwarder that relays connections over vsock.
///
/// Thread-safe: all mutation goes through `RwLock<HashMap>`.
pub struct VsockPortForwarder {
    connector: Arc<dyn VsockConnector>,
    rules: Arc<RwLock<HashMap<SocketAddr, ActiveRule>>>,
}

impl VsockPortForwarder {
    /// Creates a new forwarder with the given vsock connector.
    pub fn new(connector: Arc<dyn VsockConnector>) -> Self {
        Self {
            connector,
            rules: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Adds a TCP port forwarding rule and starts listening.
    ///
    /// `host_addr` is the local address to bind (e.g. `127.0.0.1:8080`).
    /// `target_ip` and `target_port` are what the guest agent connects to
    /// (typically `127.0.0.1` and the container's published port).
    pub async fn add_rule(
        &self,
        host_addr: SocketAddr,
        target_ip: Ipv4Addr,
        target_port: u16,
    ) -> Result<(), Error> {
        let mut rules = self.rules.write().await;
        if rules.contains_key(&host_addr) {
            return Err(Error::DuplicateRule(host_addr));
        }

        let listener = TcpListener::bind(host_addr).await?;
        let actual_addr = listener.local_addr()?;
        tracing::info!(
            "Port forward: TCP {} → guest {}:{}",
            actual_addr,
            target_ip,
            target_port,
        );

        let cancel = CancellationToken::new();
        let connector = Arc::clone(&self.connector);
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            listener_loop(listener, connector, target_ip, target_port, cancel_clone).await;
        });

        rules.insert(host_addr, ActiveRule { handle, cancel });
        Ok(())
    }

    /// Removes a port forwarding rule and stops its listener.
    pub async fn remove_rule(&self, host_addr: &SocketAddr) {
        let mut rules = self.rules.write().await;
        if let Some(rule) = rules.remove(host_addr) {
            rule.cancel.cancel();
            rule.handle.abort();
            tracing::info!("Port forward: removed {}", host_addr);
        }
    }

    /// Stops all port forwarding rules.
    pub async fn stop_all(&self) {
        let mut rules = self.rules.write().await;
        for (addr, rule) in rules.drain() {
            rule.cancel.cancel();
            rule.handle.abort();
            tracing::debug!("Port forward: stopped {}", addr);
        }
    }
}

/// Accepts TCP connections and relays each one over vsock.
async fn listener_loop(
    listener: TcpListener,
    connector: Arc<dyn VsockConnector>,
    target_ip: Ipv4Addr,
    target_port: u16,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let connector = Arc::clone(&connector);
                        tokio::spawn(async move {
                            if let Err(e) = relay_connection(
                                stream, connector, target_ip, target_port,
                            ).await {
                                tracing::debug!(
                                    "Port forward {peer} → {target_ip}:{target_port}: {e}"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Port forward accept error: {e}");
                    }
                }
            }
        }
    }
}

/// Relays a single TCP connection over vsock to the guest.
async fn relay_connection(
    tcp: tokio::net::TcpStream,
    connector: Arc<dyn VsockConnector>,
    target_ip: Ipv4Addr,
    target_port: u16,
) -> Result<(), Error> {
    tcp.set_nodelay(true)?;

    // Open vsock connection to the guest port forwarding service.
    // This is a blocking call (socketpair creation + OP_REQUEST injection),
    // so we run it on a blocking thread.
    let connector2 = Arc::clone(&connector);
    let (fd, host_port) =
        tokio::task::spawn_blocking(move || connector2.connect(PORT_FORWARD_VSOCK_PORT))
            .await
            .map_err(|e| Error::VsockConnect(e.to_string()))?
            .map_err(Error::VsockConnect)?;

    // Wrap the raw fd in a tokio AsyncFd for non-blocking I/O.
    // SAFETY: fd is a valid, owned socket from the connector.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true)?;
    let mut vsock = tokio::net::UnixStream::from_std(std_stream)?;

    // Send the 6-byte target header.
    let header = protocol::encode_header(target_ip, target_port);
    vsock.write_all(&header).await?;

    // Read 1-byte status.
    let mut status_buf = [0u8; 1];
    vsock.read_exact(&mut status_buf).await?;

    if status_buf[0] != protocol::STATUS_OK {
        return Err(Error::Handshake {
            status: status_buf[0],
            desc: protocol::status_description(status_buf[0]),
        });
    }

    // Try to promote host→guest to inline inject (bypasses socketpair).
    let tcp_std = tcp.into_std().map_err(Error::Io)?;
    tcp_std.set_nonblocking(true).map_err(Error::Io)?;

    if let Ok(inline_clone) = tcp_std.try_clone() {
        let promoted = tokio::task::spawn_blocking(move || {
            connector.promote_inline(inline_clone, host_port, PORT_FORWARD_VSOCK_PORT)
        })
        .await
        .unwrap_or(false);

        if promoted {
            // Host→guest: handled by vsock-rx worker (inline inject).
            // Guest→host: half-duplex relay via socketpair.
            tracing::info!(
                "Port forward {}:{} promoted to inline inject (host_port={})",
                target_ip,
                target_port,
                host_port,
            );
            let tcp_g2h = tokio::net::TcpStream::from_std(tcp_std).map_err(Error::Io)?;
            let (mut vsock_read, _vsock_write) = tokio::io::split(vsock);
            let (_tcp_read, mut tcp_write) = tokio::io::split(tcp_g2h);
            const RELAY_BUF: usize = 256 * 1024;
            let g2h = tokio::io::copy_buf(
                &mut tokio::io::BufReader::with_capacity(RELAY_BUF, &mut vsock_read),
                &mut tcp_write,
            )
            .await?;
            tracing::debug!(
                "Port forward {}:{} done (inline): guest→host={g2h}",
                target_ip,
                target_port,
            );
            return Ok(());
        }
    }

    // Fallback: full bidirectional relay via socketpair.
    let mut tcp = tokio::net::TcpStream::from_std(tcp_std).map_err(Error::Io)?;
    const RELAY_BUF: usize = 256 * 1024;
    let (v2t, t2v) =
        tokio::io::copy_bidirectional_with_sizes(&mut tcp, &mut vsock, RELAY_BUF, RELAY_BUF)
            .await?;

    tracing::debug!(
        "Port forward {}:{} done: host→guest={v2t} guest→host={t2v}",
        target_ip,
        target_port,
    );
    Ok(())
}
