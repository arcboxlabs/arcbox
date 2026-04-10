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

    /// Vsock buffer target. Child sockets inherit from the listener,
    /// so OP_RESPONSE carries the large buf_alloc immediately.
    /// NOTE: 8 MiB caused guest kernel to stop sending CreditUpdates.
    /// Using 2 MiB as a safe value pending kernel investigation.
    const VSOCK_BUF_SIZE: usize = 2 * 1024 * 1024;

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
        use std::os::unix::io::AsRawFd;

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

        // Enlarge TCP buffers to prevent backpressure from stalling the
        // vsock credit flow. Default SO_SNDBUF (~16 KiB) fills instantly
        // at 10 Gbps, blocking the agent's relay and stopping CreditUpdates.
        {
            use std::os::unix::io::AsRawFd;
            let tcp_fd = tcp.as_raw_fd();
            let buf_size: libc::c_int = 4 * 1024 * 1024;
            for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
                unsafe {
                    libc::setsockopt(
                        tcp_fd,
                        libc::SOL_SOCKET,
                        opt,
                        std::ptr::addr_of!(buf_size).cast(),
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
            }
        }

        // Use splice via pipe for kernel-to-kernel data transfer.
        // vsock→pipe uses copy_splice_read (1 kernel copy), pipe→tcp
        // uses MSG_SPLICE_PAGES (zero-copy). Eliminates userspace relay.
        let vsock_fd = vsock.as_raw_fd();
        let tcp_fd = tcp.as_raw_fd();

        // Keep the streams alive so fds aren't closed.
        let _vsock_guard = vsock;
        let _tcp_guard = tcp;

        tokio::task::spawn_blocking(move || splice_relay_fds(vsock_fd, tcp_fd))
            .await
            .context("splice relay join")??;

        Ok(())
    }

    /// Bidirectional splice relay between two fds via a kernel pipe.
    ///
    /// Each direction uses `splice(src → pipe) + splice(pipe → dst)`.
    /// The pipe→tcp leg is zero-copy (MSG_SPLICE_PAGES). The vsock→pipe
    /// leg uses the kernel's copy_splice_read fallback (one kernel copy,
    /// no userspace copy). Net: eliminates all userspace memcpy.
    ///
    /// Runs on a blocking thread because splice is a blocking syscall.
    fn splice_relay_fds(vsock_fd: i32, tcp_fd: i32) -> anyhow::Result<()> {
        // Set both to blocking for splice.
        unsafe {
            let flags = libc::fcntl(vsock_fd, libc::F_GETFL);
            libc::fcntl(vsock_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            let flags = libc::fcntl(tcp_fd, libc::F_GETFL);
            libc::fcntl(tcp_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }

        // Create pipes for each direction.
        let mut pipe_v2t = [0i32; 2]; // vsock → tcp
        let mut pipe_t2v = [0i32; 2]; // tcp → vsock
        if unsafe { libc::pipe(pipe_v2t.as_mut_ptr()) } != 0
            || unsafe { libc::pipe(pipe_t2v.as_mut_ptr()) } != 0
        {
            anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
        }

        // Increase pipe capacity for throughput.
        unsafe {
            libc::fcntl(pipe_v2t[0], libc::F_SETPIPE_SZ, 1024 * 1024);
            libc::fcntl(pipe_t2v[0], libc::F_SETPIPE_SZ, 1024 * 1024);
        }

        const SPLICE_LEN: usize = 1024 * 1024;
        const SPLICE_FLAGS: libc::c_uint = libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE;

        // Spawn one thread per direction.
        let v2t = std::thread::Builder::new()
            .name("splice-v2t".into())
            .spawn(move || -> anyhow::Result<u64> {
                let mut total = 0u64;
                loop {
                    let n = unsafe {
                        libc::splice(
                            vsock_fd,
                            std::ptr::null_mut(),
                            pipe_v2t[1],
                            std::ptr::null_mut(),
                            SPLICE_LEN,
                            SPLICE_FLAGS,
                        )
                    };
                    if n <= 0 {
                        break;
                    }
                    let mut remaining = n as usize;
                    while remaining > 0 {
                        let w = unsafe {
                            libc::splice(
                                pipe_v2t[0],
                                std::ptr::null_mut(),
                                tcp_fd,
                                std::ptr::null_mut(),
                                remaining,
                                SPLICE_FLAGS,
                            )
                        };
                        if w <= 0 {
                            break;
                        }
                        remaining -= w as usize;
                    }
                    total += n as u64;
                }
                unsafe {
                    libc::close(pipe_v2t[0]);
                    libc::close(pipe_v2t[1]);
                }
                Ok(total)
            })?;

        // tcp → vsock direction in current thread.
        let mut t2v_total = 0u64;
        loop {
            let n = unsafe {
                libc::splice(
                    tcp_fd,
                    std::ptr::null_mut(),
                    pipe_t2v[1],
                    std::ptr::null_mut(),
                    SPLICE_LEN,
                    SPLICE_FLAGS,
                )
            };
            if n <= 0 {
                break;
            }
            let mut remaining = n as usize;
            while remaining > 0 {
                let w = unsafe {
                    libc::splice(
                        pipe_t2v[0],
                        std::ptr::null_mut(),
                        vsock_fd,
                        std::ptr::null_mut(),
                        remaining,
                        SPLICE_FLAGS,
                    )
                };
                if w <= 0 {
                    break;
                }
                remaining -= w as usize;
            }
            t2v_total += n as u64;
        }
        unsafe {
            libc::close(pipe_t2v[0]);
            libc::close(pipe_t2v[1]);
        }

        let v2t_total = v2t
            .join()
            .map_err(|_| anyhow::anyhow!("splice-v2t panicked"))??;

        tracing::debug!("splice relay done: vsock→tcp={v2t_total} tcp→vsock={t2v_total}",);
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
