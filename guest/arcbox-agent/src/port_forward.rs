//! Vsock-based TCP port forwarding proxy.
//!
//! Listens on a dedicated vsock port and relays TCP connections into the
//! guest. For each accepted vsock connection the host sends a 6-byte
//! header that names the target, the agent connects, and then data flows
//! bidirectionally with zero intermediate copies.
//!
//! Protocol:
//! ```text
//! Host → Guest:  [target_ip: 4 bytes][target_port: 2 bytes BE]
//! Guest → Host:  [status: 1 byte]  (0x00 = ok)
//! Then:          raw bidirectional relay
//! ```
//!
//! This replaces the virtio-net based port forwarding path
//! (smoltcp → frame construction → descriptor injection) with a
//! single-copy vsock pipe.

#[cfg(target_os = "linux")]
mod inner {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

    use arcbox_constants::ports::PORT_FORWARD_VSOCK_PORT;

    /// Status codes sent back to the host after the connect attempt.
    const STATUS_OK: u8 = 0x00;
    const STATUS_REFUSED: u8 = 0x01;
    const STATUS_UNREACHABLE: u8 = 0x02;
    const STATUS_ERROR: u8 = 0xFF;

    /// Runs the port forwarding proxy. Never returns under normal operation.
    pub async fn run() -> Result<()> {
        let mut listener = bind_with_retry(PORT_FORWARD_VSOCK_PORT).await?;
        tracing::info!(
            "Port forward proxy listening on vsock port {}",
            PORT_FORWARD_VSOCK_PORT
        );

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::debug!("Port forward accepted from {:?}", peer_addr);
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream).await {
                            tracing::debug!("Port forward session ended: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Port forward accept failed: {}", e);
                }
            }
        }
    }

    // Linux UAPI constants for vsock-specific buffer control.
    // These options live at the AF_VSOCK socket level, not SOL_VSOCK.
    // They directly control the virtio_vsock driver's buf_alloc advertised
    // to the host, unlike generic SO_RCVBUF.
    const SO_VM_SOCKETS_BUFFER_SIZE: libc::c_int = 0;
    const SO_VM_SOCKETS_BUFFER_MIN_SIZE: libc::c_int = 1;
    const SO_VM_SOCKETS_BUFFER_MAX_SIZE: libc::c_int = 2;

    /// Relay buffer size for copy_bidirectional_with_sizes.
    /// 256 KiB reduces wakeup frequency at high throughput compared
    /// to the default 8 KiB.
    const RELAY_BUF_SIZE: usize = 256 * 1024;

    /// Vsock buffer target: 8 MiB. Child sockets inherit from the listener,
    /// so OP_RESPONSE carries the large buf_alloc immediately.
    const VSOCK_BUF_SIZE: usize = 8 * 1024 * 1024;

    /// Sets the vsock-specific buffer sizes that directly control the
    /// guest kernel's `buf_alloc` advertised to the host.
    /// Called on the LISTENER fd so child sockets inherit the large buffer.
    fn set_vsock_fd_buffers(fd: std::os::unix::io::RawFd, size: usize) {
        let size = size as u64;
        for (opt, name) in [
            (SO_VM_SOCKETS_BUFFER_MAX_SIZE, "BUFFER_MAX_SIZE"),
            (SO_VM_SOCKETS_BUFFER_SIZE, "BUFFER_SIZE"),
            (SO_VM_SOCKETS_BUFFER_MIN_SIZE, "BUFFER_MIN_SIZE"),
        ] {
            let ret = unsafe {
                libc::setsockopt(
                    fd,
                    libc::AF_VSOCK,
                    opt,
                    std::ptr::addr_of!(size).cast(),
                    std::mem::size_of::<u64>() as libc::socklen_t,
                )
            };
            if ret != 0 {
                tracing::warn!(
                    "setsockopt(AF_VSOCK, {name}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        let mut effective = 0_u64;
        let mut len = std::mem::size_of::<u64>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockopt(
                fd,
                libc::AF_VSOCK,
                SO_VM_SOCKETS_BUFFER_SIZE,
                std::ptr::addr_of_mut!(effective).cast(),
                std::ptr::addr_of_mut!(len),
            )
        };
        if ret == 0 {
            tracing::info!(
                "Port forward vsock buffer configured: requested={size} effective={effective}"
            );
        } else {
            tracing::warn!(
                "getsockopt(AF_VSOCK, BUFFER_SIZE) failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Handles a single forwarded connection.
    async fn handle(mut vsock: VsockStream) -> Result<()> {
        // Fallback: also set on accepted socket in case listener inheritance
        // didn't apply (older kernels). The OP_RESPONSE already carries the
        // large buf_alloc from the listener.
        use std::os::unix::io::AsRawFd;
        set_vsock_fd_buffers(vsock.as_raw_fd(), VSOCK_BUF_SIZE);

        // Read 6-byte header: [ip: 4][port: 2 BE]
        let mut header = [0u8; 6];
        vsock
            .read_exact(&mut header)
            .await
            .context("read port forward header")?;

        let target_ip = Ipv4Addr::new(header[0], header[1], header[2], header[3]);
        let target_port = u16::from_be_bytes([header[4], header[5]]);
        let target_addr = SocketAddrV4::new(target_ip, target_port);

        tracing::debug!("Port forward connecting to {}", target_addr);

        // Connect to the target inside the guest.
        let mut tcp = match TcpStream::connect(target_addr).await {
            Ok(stream) => {
                vsock.write_all(&[STATUS_OK]).await?;
                stream
            }
            Err(e) => {
                let status = match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => STATUS_REFUSED,
                    std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::TimedOut => STATUS_UNREACHABLE,
                    _ => STATUS_ERROR,
                };
                let _ = vsock.write_all(&[status]).await;
                return Err(e).context(format!("connect to {target_addr}"));
            }
        };

        tcp.set_nodelay(true)?;

        let (v2t, t2v) = tokio::io::copy_bidirectional_with_sizes(
            &mut vsock,
            &mut tcp,
            RELAY_BUF_SIZE,
            RELAY_BUF_SIZE,
        )
        .await
        .context("port forward relay")?;

        tracing::debug!(
            "Port forward {} done: vsock→tcp={v2t} tcp→vsock={t2v}",
            target_addr,
        );
        Ok(())
    }

    /// Binds a vsock listener with exponential backoff retries.
    async fn bind_with_retry(port: u32) -> Result<VsockListener> {
        const INITIAL_DELAY_MS: u64 = 120;
        const MAX_DELAY_MS: u64 = 2_000;
        let mut delay = INITIAL_DELAY_MS;

        loop {
            match VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port)) {
                Ok(l) => {
                    // Set buffer on listener fd — child sockets inherit,
                    // so OP_RESPONSE immediately carries the large buf_alloc.
                    use std::os::unix::io::AsRawFd;
                    set_vsock_fd_buffers(l.as_raw_fd(), VSOCK_BUF_SIZE);
                    return Ok(l);
                }
                Err(e) => {
                    tracing::warn!(
                        port,
                        "port forward vsock bind failed ({e}), retrying in {delay}ms"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 3 / 2).min(MAX_DELAY_MS);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use inner::run;
