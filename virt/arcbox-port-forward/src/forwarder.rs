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
                                tracing::warn!(
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

    // SAFETY: fd is a valid, owned socket from the connector.
    let vsock_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    // Blocking mode for handshake.

    // Send the 6-byte target header (blocking).
    {
        use std::io::Write;
        let header = protocol::encode_header(target_ip, target_port);
        (&vsock_stream).write_all(&header)?;
    }

    // Read 1-byte status (blocking).
    {
        use std::io::Read;
        let mut status_buf = [0u8; 1];
        (&vsock_stream).read_exact(&mut status_buf)?;
        if status_buf[0] != protocol::STATUS_OK {
            return Err(Error::Handshake {
                status: status_buf[0],
                desc: protocol::status_description(status_buf[0]),
            });
        }
    }

    let _ = (&connector, host_port);

    // Convert TCP stream to std blocking.
    let tcp_std = tcp.into_std()?;
    tcp_std.set_nonblocking(false)?;

    // Enlarge TCP recv buffer to prevent zero-window stalls. The
    // default macOS SO_RCVBUF is 128 KiB — at 8 Gbps that fills in
    // ~128μs, shorter than one INJECT_YIELD cycle. The TCP window
    // closes, iperf enters persist mode with exponential backoff,
    // and recovery stalls indefinitely.
    {
        use std::os::unix::io::AsRawFd;
        let buf_size: libc::c_int = 16 * 1024 * 1024; // 16 MiB
        unsafe {
            libc::setsockopt(
                tcp_std.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                std::ptr::addr_of!(buf_size).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    // Blocking bidirectional relay — eliminates tokio from the data path.
    let relay_start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || -> io::Result<(u64, u64)> {
        use std::io::{Read, Write};

        let vsock_r = vsock_stream;
        let vsock_w = vsock_r.try_clone()?;
        let tcp_r = tcp_std;
        let tcp_w = tcp_r.try_clone()?;

        // tcp → vsock (host→guest): decoupled via channel so TCP read
        // never blocks on socketpair write. This keeps the TCP recv
        // buffer drained and the TCP window open even during brief
        // socketpair backpressure.
        // Unbounded channel: the TCP reader MUST NOT block. macOS caps
        // socket buffers at kern.ipc.maxsockbuf (8MB), so any bounded
        // channel that fills during a >~40ms socketpair stall causes
        // the TCP window to close, triggering persist-mode backoff that
        // never fully recovers. Steady-state memory is near-zero (the
        // socketpair writer drains as fast as data arrives). Only peaks
        // during vsock credit/descriptor backpressure bursts.
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

        // TCP reader thread: drains TCP recv buffer into channel.
        let tcp_reader = std::thread::Builder::new().name("pf-tcp-rd".into()).spawn(
            move || -> io::Result<u64> {
                let mut buf = vec![0u8; 256 * 1024];
                let mut total = 0u64;
                let mut tcp_r = tcp_r;
                loop {
                    let n = tcp_r.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Ok(total)
            },
        )?;

        // Socketpair writer thread: drains channel into socketpair.
        let t2v = std::thread::Builder::new().name("pf-sp-wr".into()).spawn(
            move || -> io::Result<u64> {
                let mut total = 0u64;
                let mut vsock_w = vsock_w;
                while let Ok(data) = rx.recv() {
                    vsock_w.write_all(&data)?;
                    total += data.len() as u64;
                }
                Ok(total)
            },
        )?;

        // vsock → tcp (guest→host) in current thread
        let mut buf = vec![0u8; 256 * 1024];
        let mut v2t_total = 0u64;
        let mut vsock_r = vsock_r;
        let mut tcp_w = tcp_w;
        loop {
            let n = vsock_r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            tcp_w.write_all(&buf[..n])?;
            v2t_total += n as u64;
        }

        let tcp_rd_total = tcp_reader
            .join()
            .map_err(|_| io::Error::other("pf-tcp-rd panicked"))??;
        let sp_wr_total = t2v
            .join()
            .map_err(|_| io::Error::other("pf-sp-wr panicked"))??;
        // tcp_rd_total = bytes read from TCP, sp_wr_total = bytes
        // written to socketpair. They should match unless channel
        // was dropped early.
        let _ = tcp_rd_total;
        Ok((sp_wr_total, v2t_total))
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?;

    let elapsed = relay_start.elapsed();
    match &result {
        Ok((t2v, v2t)) => {
            tracing::info!(
                "Port forward {}:{} done after {:.1}s: host→guest={t2v} guest→host={v2t}",
                target_ip,
                target_port,
                elapsed.as_secs_f64(),
            );
        }
        Err(e) => {
            tracing::warn!(
                "Port forward {}:{} FAILED after {:.1}s: {e}",
                target_ip,
                target_port,
                elapsed.as_secs_f64(),
            );
        }
    }
    result?;
    Ok(())
}
