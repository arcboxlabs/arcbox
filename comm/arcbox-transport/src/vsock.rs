//! Virtio socket (vsock) transport.
//!
//! Provides high-performance communication between host and guest VM.
//!
//! ## Platform Support
//!
//! - **Linux**: Uses `AF_VSOCK` socket family directly via nix crate.
//!   Standard `bind()` and `connect()` operations work as expected.
//!
//! - **macOS**: Uses Virtualization.framework (`VZVirtioSocketDevice`).
//!   Requires integration with the hypervisor layer:
//!   - For connections: Use [`VsockTransport::from_raw_fd()`] with an fd
//!     obtained from `VirtioSocketDevice::connect()`.
//!   - For listening: Use [`VsockListener::from_channel()`] with a channel
//!     connected to `VirtioSocketListener::accept()`.
//!
//! ## CID (Context ID) Values
//!
//! - `VMADDR_CID_HYPERVISOR` (0): Reserved for hypervisor
//! - `VMADDR_CID_LOCAL` (1): Local communication
//! - `VMADDR_CID_HOST` (2): Host (from guest perspective)
//! - 3+: Guest VMs
//!
//! ## macOS Usage Example
//!
//! ```rust,ignore
//! use arcbox_vz::{VirtualMachine, VirtioSocketDevice};
//! use arcbox_transport::vsock::{VsockListener, VsockTransport, VsockAddr, IncomingVsockConnection};
//! use tokio::sync::mpsc;
//!
//! // Get socket device from running VM
//! let vm: VirtualMachine = /* ... */;
//! let device = &vm.socket_devices()[0];
//!
//! // === Connecting to guest ===
//! let conn = device.connect(1024).await?;
//! let fd = conn.into_raw_fd();
//! let transport = VsockTransport::from_raw_fd(fd, VsockAddr::new(cid, 1024))?;
//!
//! // === Listening for guest connections ===
//! let mut vz_listener = device.listen(1024)?;
//! let (tx, rx) = mpsc::unbounded_channel();
//!
//! // Bridge VZ listener to transport layer
//! tokio::spawn(async move {
//!     loop {
//!         match vz_listener.accept().await {
//!             Ok(conn) => {
//!                 let incoming = IncomingVsockConnection {
//!                     fd: conn.into_raw_fd(),
//!                     source_port: conn.source_port(),
//!                     destination_port: conn.destination_port(),
//!                 };
//!                 if tx.send(incoming).is_err() {
//!                     break;
//!                 }
//!             }
//!             Err(_) => break,
//!         }
//!     }
//! });
//!
//! let mut listener = VsockListener::from_channel(1024, rx);
//! let transport = listener.accept().await?;
//! ```

use crate::error::{Result, TransportError};
use crate::{Transport, TransportListener};
use async_trait::async_trait;

/// Maximum allowed frame payload size (16 MiB).
///
/// Frames larger than this are rejected before allocation to prevent a
/// misbehaving or compromised guest from triggering unbounded memory growth on
/// the host.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
use bytes::Bytes;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::io::{ReadHalf, WriteHalf};

/// Default port for `ArcBox` agent communication.
pub use arcbox_constants::ports::AGENT_PORT as DEFAULT_AGENT_PORT;

/// Vsock address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockAddr {
    /// Context ID.
    pub cid: u32,
    /// Port number.
    pub port: u32,
}

impl VsockAddr {
    /// Hypervisor CID (reserved).
    pub const CID_HYPERVISOR: u32 = 0;
    /// Local CID.
    pub const CID_LOCAL: u32 = 1;
    /// Host CID (from guest perspective).
    pub const CID_HOST: u32 = 2;
    /// Any CID (for binding).
    pub const CID_ANY: u32 = u32::MAX;

    /// Creates a new vsock address.
    #[must_use]
    pub const fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }

    /// Creates an address for the host (from guest perspective).
    #[must_use]
    pub const fn host(port: u32) -> Self {
        Self::new(Self::CID_HOST, port)
    }

    /// Creates an address for any CID (for binding).
    #[must_use]
    pub const fn any(port: u32) -> Self {
        Self::new(Self::CID_ANY, port)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VsockStream
// ─────────────────────────────────────────────────────────────────────────────

/// Async vsock stream.
///
/// Wraps a connected vsock file descriptor and implements [`AsyncRead`] +
/// [`AsyncWrite`] via [`AsyncFd`] (epoll on Linux, kqueue on macOS).
/// This replaces the previous per-platform busy-polling approach and enables
/// `tokio::io::split()` for full-duplex use.
pub struct VsockStream {
    inner: AsyncFd<OwnedFd>,
}

impl VsockStream {
    /// Sets a file descriptor to non-blocking mode.
    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: fd is valid for the duration of the fcntl calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is valid; O_NONBLOCK is a safe flag to set.
        let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Creates a vsock stream from an [`OwnedFd`].
    ///
    /// Sets `O_NONBLOCK` and registers the fd with the tokio reactor.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Self::set_nonblocking(fd.as_raw_fd())?;
        Ok(Self {
            inner: AsyncFd::new(fd)?,
        })
    }

    /// Creates a vsock stream from a raw file descriptor, taking ownership.
    ///
    /// # Safety
    /// The caller must ensure `fd` is a valid connected vsock file descriptor.
    pub unsafe fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
        // SAFETY: caller guarantees fd is valid.
        Self::from_fd(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Returns the raw file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| {
                // SAFETY: fd is valid and owned by AsyncFd for the duration of
                // this call; `unfilled` is a valid writable buffer slice.
                loop {
                    let n = unsafe {
                        libc::read(
                            inner.as_raw_fd(),
                            unfilled.as_mut_ptr().cast::<libc::c_void>(),
                            unfilled.len(),
                        )
                    };
                    if n >= 0 {
                        return Ok(n as usize);
                    }
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                    // EINTR: retry the syscall immediately.
                }
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                // SAFETY: fd is valid and owned by AsyncFd for the duration of
                // this call; `buf` is a valid readable buffer slice.
                loop {
                    let n = unsafe {
                        libc::write(
                            inner.as_raw_fd(),
                            buf.as_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if n >= 0 {
                        return Ok(n as usize);
                    }
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                    // EINTR: retry the syscall immediately.
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                // SAFETY: fd is valid; SHUT_WR is a safe shutdown direction that
                // sends FIN to the peer without closing the read side.
                let ret = unsafe { libc::shutdown(inner.as_raw_fd(), libc::SHUT_WR) };
                if ret < 0 {
                    let err = io::Error::last_os_error();
                    // Treat already-shutdown or not-connected sockets as
                    // successfully shut down to make the operation idempotent.
                    if matches!(err.raw_os_error(), Some(libc::ENOTCONN | libc::EINVAL)) {
                        Ok(())
                    } else {
                        Err(err)
                    }
                } else {
                    Ok(())
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => {}
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Platform-specific connection setup
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};
    use std::mem;

    /// Raw `sockaddr_vm` structure for vsock.
    #[repr(C)]
    struct SockaddrVm {
        svm_family: libc::sa_family_t,
        svm_reserved1: u16,
        svm_port: u32,
        svm_cid: u32,
        svm_flags: u8,
        svm_zero: [u8; 3],
    }

    impl SockaddrVm {
        fn new(cid: u32, port: u32) -> Self {
            Self {
                svm_family: libc::AF_VSOCK as libc::sa_family_t,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: cid,
                svm_flags: 0,
                svm_zero: [0; 3],
            }
        }
    }

    /// Creates a vsock socket.
    pub fn create_socket() -> Result<OwnedFd> {
        let fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .map_err(|e| TransportError::io(e.into()))?;
        Ok(fd)
    }

    /// Connects to a vsock address and returns a ready [`VsockStream`].
    pub fn connect_vsock(addr: VsockAddr) -> Result<VsockStream> {
        let fd = create_socket()?;

        let sockaddr = SockaddrVm::new(addr.cid, addr.port);
        let sockaddr_ptr = &sockaddr as *const SockaddrVm as *const libc::sockaddr;

        // SAFETY: sockaddr_ptr points to a correctly initialised SockaddrVm.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                sockaddr_ptr,
                mem::size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };

        if result < 0 {
            let err = io::Error::last_os_error();
            return Err(TransportError::ConnectionRefused(err.to_string()));
        }

        VsockStream::from_fd(fd).map_err(TransportError::io)
    }

    /// Binds to a vsock port and returns the listening socket fd.
    pub fn bind_vsock(port: u32) -> Result<OwnedFd> {
        let fd = create_socket()?;

        let sockaddr = SockaddrVm::new(VsockAddr::CID_ANY, port);
        let sockaddr_ptr = &sockaddr as *const SockaddrVm as *const libc::sockaddr;

        // SAFETY: sockaddr_ptr points to a correctly initialised SockaddrVm.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                sockaddr_ptr,
                mem::size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(TransportError::io(io::Error::last_os_error()));
        }

        // SAFETY: fd is a valid bound socket.
        let result = unsafe { libc::listen(fd.as_raw_fd(), 128) };
        if result < 0 {
            return Err(TransportError::io(io::Error::last_os_error()));
        }

        Ok(fd)
    }

    /// Accepts a connection on a vsock listener.
    pub fn accept_vsock(listener_fd: &OwnedFd) -> Result<(VsockStream, VsockAddr)> {
        let mut sockaddr = SockaddrVm::new(0, 0);
        let mut len = mem::size_of::<SockaddrVm>() as libc::socklen_t;

        // SAFETY: listener_fd is a valid socket; sockaddr is correctly sized.
        // Use accept4 with SOCK_CLOEXEC to prevent fd leaks into child processes.
        let fd = unsafe {
            libc::accept4(
                listener_fd.as_raw_fd(),
                &mut sockaddr as *mut SockaddrVm as *mut libc::sockaddr,
                &mut len,
                libc::SOCK_CLOEXEC,
            )
        };

        if fd < 0 {
            return Err(TransportError::io(io::Error::last_os_error()));
        }

        // SAFETY: fd was just returned from accept4() and is valid.
        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = VsockAddr::new(sockaddr.svm_cid, sockaddr.svm_port);

        Ok((
            VsockStream::from_fd(owned_fd).map_err(TransportError::io)?,
            addr,
        ))
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    use super::*;
    use tokio::sync::mpsc;

    /// Information about an incoming vsock connection.
    #[derive(Debug, Clone)]
    pub struct IncomingVsockConnection {
        /// File descriptor for the connection.
        pub fd: RawFd,
        /// Source port (guest port).
        pub source_port: u32,
        /// Destination port (host port).
        pub destination_port: u32,
    }

    /// On macOS, direct vsock connection is not supported.
    /// Use `VsockTransport::from_raw_fd()` with a fd obtained from the hypervisor layer.
    pub fn connect_vsock(_addr: VsockAddr) -> Result<VsockStream> {
        Err(TransportError::Protocol(
            "macOS vsock requires connection through hypervisor".to_string(),
        ))
    }

    /// Vsock listener handle for macOS.
    ///
    /// On macOS, vsock listening is done through `VZVirtioSocketDevice`.
    /// This handle wraps a channel that receives incoming connections
    /// from the Virtualization.framework layer.
    #[allow(dead_code)]
    pub struct VsockListenerHandle {
        /// Port number being listened on.
        port: u32,
        /// Channel receiver for incoming connections.
        receiver: mpsc::UnboundedReceiver<IncomingVsockConnection>,
    }

    #[allow(dead_code)]
    impl VsockListenerHandle {
        /// Creates a new listener handle from a channel.
        pub const fn new(
            port: u32,
            receiver: mpsc::UnboundedReceiver<IncomingVsockConnection>,
        ) -> Self {
            Self { port, receiver }
        }

        /// Returns the port number.
        pub const fn port(&self) -> u32 {
            self.port
        }

        /// Accepts an incoming connection.
        pub async fn accept(&mut self) -> Result<(VsockStream, VsockAddr)> {
            match self.receiver.recv().await {
                Some(conn) => {
                    // SAFETY: conn.fd comes from VZVirtioSocketDevice and is a
                    // valid connected vsock file descriptor.
                    let stream =
                        unsafe { VsockStream::from_raw_fd(conn.fd) }.map_err(TransportError::io)?;
                    let addr = VsockAddr::new(VsockAddr::CID_ANY, conn.source_port);
                    Ok((stream, addr))
                }
                None => Err(TransportError::ConnectionReset),
            }
        }

        /// Tries to accept a connection without blocking.
        pub fn try_accept(&mut self) -> Option<Result<(VsockStream, VsockAddr)>> {
            match self.receiver.try_recv() {
                Ok(conn) => {
                    // SAFETY: conn.fd comes from VZVirtioSocketDevice and is a
                    // valid connected vsock file descriptor.
                    let stream =
                        unsafe { VsockStream::from_raw_fd(conn.fd) }.map_err(TransportError::io);
                    match stream {
                        Ok(s) => {
                            let addr = VsockAddr::new(VsockAddr::CID_ANY, conn.source_port);
                            Some(Ok((s, addr)))
                        }
                        Err(e) => Some(Err(e)),
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => None,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    Some(Err(TransportError::ConnectionReset))
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared framing helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Reads one length-prefixed frame from `reader`.
///
/// Returns the complete framed bytes (4-byte BE length prefix + payload).
/// Rejects frames larger than [`MAX_FRAME_SIZE`] before allocating.
async fn read_framed<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Bytes> {
    // Read 4-byte big-endian length prefix.
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(TransportError::io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(TransportError::Protocol(format!(
            "frame too large: {len} bytes (max {MAX_FRAME_SIZE})"
        )));
    }
    tracing::debug!("vsock read_framed: message length={}", len);

    // Single allocation: length prefix + payload.
    let mut buf = vec![0u8; 4 + len];
    buf[..4].copy_from_slice(&len_buf);
    reader
        .read_exact(&mut buf[4..])
        .await
        .map_err(TransportError::io)?;
    Ok(Bytes::from(buf))
}

// ─────────────────────────────────────────────────────────────────────────────
// VsockSender / VsockReceiver — full-duplex split types
// ─────────────────────────────────────────────────────────────────────────────

/// Write half of a split [`VsockTransport`].
///
/// Obtained via [`VsockTransport::into_split`]. Owns the write side of the
/// underlying stream. The caller is responsible for providing already-framed
/// data (e.g. from `AgentClient::build_message`).
pub struct VsockSender {
    writer: WriteHalf<VsockStream>,
}

impl VsockSender {
    /// Sends a framed message.
    ///
    /// `data` must already include the wire framing produced by the upper
    /// layer (e.g. `AgentClient::build_message`).
    pub async fn send(&mut self, data: Bytes) -> Result<()> {
        self.writer
            .write_all(&data)
            .await
            .map_err(TransportError::io)
    }
}

/// Read half of a split [`VsockTransport`].
///
/// Obtained via [`VsockTransport::into_split`]. Owns the read side of the
/// underlying stream and reads one length-prefixed frame per call, matching
/// the framing of [`VsockTransport::recv`].
pub struct VsockReceiver {
    reader: ReadHalf<VsockStream>,
}

impl VsockReceiver {
    /// Receives one framed message.
    ///
    /// Returns the complete framed bytes (4-byte BE length prefix + payload),
    /// matching the format returned by [`VsockTransport::recv`].
    pub async fn recv(&mut self) -> Result<Bytes> {
        read_framed(&mut self.reader).await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VsockTransport
// ─────────────────────────────────────────────────────────────────────────────

/// Vsock transport for host-guest communication.
pub struct VsockTransport {
    addr: VsockAddr,
    stream: Option<VsockStream>,
}

impl VsockTransport {
    /// Creates a new vsock transport for the given address.
    #[must_use]
    pub const fn new(addr: VsockAddr) -> Self {
        Self { addr, stream: None }
    }

    /// Creates a transport to the host on the default agent port.
    #[must_use]
    pub const fn to_host() -> Self {
        Self::new(VsockAddr::host(DEFAULT_AGENT_PORT))
    }

    /// Returns the vsock address.
    #[must_use]
    pub const fn addr(&self) -> VsockAddr {
        self.addr
    }

    /// Creates a transport from an existing stream.
    #[must_use]
    pub fn from_stream(stream: VsockStream, addr: VsockAddr) -> Self {
        Self {
            addr,
            stream: Some(stream),
        }
    }

    /// Creates a transport from a raw file descriptor (macOS).
    ///
    /// On macOS, vsock connections are obtained through the hypervisor layer
    /// (`DarwinVm::connect_vsock`). This method allows wrapping that fd into
    /// a transport.
    ///
    /// # Arguments
    /// * `fd` - A connected vsock file descriptor from the hypervisor
    /// * `addr` - The vsock address (for tracking purposes)
    #[cfg(target_os = "macos")]
    pub fn from_raw_fd(fd: RawFd, addr: VsockAddr) -> Result<Self> {
        if fd < 0 {
            return Err(TransportError::io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid raw vsock file descriptor",
            )));
        }
        // SAFETY: fd has been validated to be non-negative and is expected to
        // be a valid connected vsock file descriptor from VZVirtioSocketDevice.
        let stream = unsafe { VsockStream::from_raw_fd(fd) }.map_err(TransportError::io)?;
        Ok(Self {
            addr,
            stream: Some(stream),
        })
    }

    /// Splits the transport into independent send and receive halves.
    ///
    /// Consumes `self`. The returned [`VsockSender`] and [`VsockReceiver`] can
    /// be moved into separate tasks for full-duplex communication. Tokio may
    /// use internal synchronization to coordinate access to the underlying
    /// stream.
    ///
    /// # Errors
    /// Returns [`TransportError::NotConnected`] if the transport is not
    /// connected.
    pub fn into_split(self) -> Result<(VsockSender, VsockReceiver)> {
        let stream = self.stream.ok_or(TransportError::NotConnected)?;
        let (reader, writer) = tokio::io::split(stream);
        Ok((VsockSender { writer }, VsockReceiver { reader }))
    }
}

#[async_trait]
impl Transport for VsockTransport {
    async fn connect(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Err(TransportError::AlreadyConnected);
        }

        #[cfg(target_os = "linux")]
        {
            let stream = linux::connect_vsock(self.addr)?;
            self.stream = Some(stream);
        }

        #[cfg(target_os = "macos")]
        {
            let stream = darwin::connect_vsock(self.addr)?;
            self.stream = Some(stream);
        }

        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.stream.take();
        Ok(())
    }

    async fn send(&mut self, data: Bytes) -> Result<()> {
        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;
        // Data already carries the wire framing (length prefix + type + payload).
        stream.write_all(&data).await.map_err(TransportError::io)
    }

    async fn recv(&mut self) -> Result<Bytes> {
        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;
        read_framed(stream).await
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VsockListener
// ─────────────────────────────────────────────────────────────────────────────

/// Vsock listener for accepting connections.
///
/// On Linux, this uses `AF_VSOCK` sockets directly.
/// On macOS, this requires a channel connected to `VZVirtioSocketDevice`.
pub struct VsockListener {
    port: u32,
    #[cfg(target_os = "linux")]
    listener_fd: Option<OwnedFd>,
    #[cfg(target_os = "macos")]
    handle: Option<darwin::VsockListenerHandle>,
}

impl VsockListener {
    /// Creates a new vsock listener on the given port.
    ///
    /// # Platform Notes
    ///
    /// - **Linux**: After calling `bind()`, the listener will be ready to accept connections.
    /// - **macOS**: Use `from_channel()` instead to provide a channel connected to
    ///   `VZVirtioSocketDevice`. Calling `bind()` on this listener will fail.
    #[must_use]
    pub const fn new(port: u32) -> Self {
        Self {
            port,
            #[cfg(target_os = "linux")]
            listener_fd: None,
            #[cfg(target_os = "macos")]
            handle: None,
        }
    }

    /// Creates a listener from a channel (macOS).
    ///
    /// On macOS, vsock listening is done through `VZVirtioSocketDevice`.
    /// This method creates a listener that receives connections from a channel
    /// bridged to the Virtualization.framework layer.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub const fn from_channel(
        port: u32,
        receiver: tokio::sync::mpsc::UnboundedReceiver<darwin::IncomingVsockConnection>,
    ) -> Self {
        Self {
            port,
            handle: Some(darwin::VsockListenerHandle::new(port, receiver)),
        }
    }

    /// Creates a listener on the default agent port.
    #[must_use]
    pub const fn default_agent() -> Self {
        Self::new(DEFAULT_AGENT_PORT)
    }

    /// Returns the port number.
    #[must_use]
    pub const fn port(&self) -> u32 {
        self.port
    }
}

#[async_trait]
impl TransportListener for VsockListener {
    type Transport = VsockTransport;

    async fn bind(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let fd = linux::bind_vsock(self.port)?;
            self.listener_fd = Some(fd);
        }

        #[cfg(target_os = "macos")]
        {
            if self.handle.is_none() {
                return Err(TransportError::Protocol(
                    "macOS vsock requires VZVirtioSocketDevice. Use VsockListener::from_channel() \
                     with a channel connected to VirtioSocketListener."
                        .to_string(),
                ));
            }
        }

        Ok(())
    }

    async fn accept(&mut self) -> Result<Self::Transport> {
        #[cfg(target_os = "linux")]
        {
            let listener_fd = self
                .listener_fd
                .as_ref()
                .ok_or(TransportError::NotConnected)?;

            let (stream, addr) = linux::accept_vsock(listener_fd)?;
            Ok(VsockTransport::from_stream(stream, addr))
        }

        #[cfg(target_os = "macos")]
        {
            let handle = self.handle.as_mut().ok_or_else(|| {
                TransportError::Protocol(
                    "macOS vsock listener not initialized. Use VsockListener::from_channel()."
                        .to_string(),
                )
            })?;

            let (stream, addr) = handle.accept().await?;
            Ok(VsockTransport::from_stream(stream, addr))
        }
    }

    async fn close(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.listener_fd.take();
        }

        #[cfg(target_os = "macos")]
        {
            self.handle.take();
        }

        Ok(())
    }
}

// Re-export macOS-specific types for use by callers.
#[cfg(target_os = "macos")]
pub use darwin::IncomingVsockConnection;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vsock_addr() {
        let addr = VsockAddr::new(3, 1234);
        assert_eq!(addr.cid, 3);
        assert_eq!(addr.port, 1234);

        let host = VsockAddr::host(DEFAULT_AGENT_PORT);
        assert_eq!(host.cid, VsockAddr::CID_HOST);
        assert_eq!(host.port, DEFAULT_AGENT_PORT);
    }

    #[test]
    fn test_vsock_transport_not_connected() {
        let transport = VsockTransport::new(VsockAddr::host(1234));
        assert!(!transport.is_connected());
    }

    #[test]
    fn test_into_split_not_connected() {
        let transport = VsockTransport::new(VsockAddr::host(1234));
        assert!(transport.into_split().is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_vsock_listener_from_channel() {
        use tokio::sync::mpsc;

        let (tx, rx) = mpsc::unbounded_channel();
        let listener = VsockListener::from_channel(1024, rx);
        assert_eq!(listener.port(), 1024);

        let conn = IncomingVsockConnection {
            fd: 42,
            source_port: 2000,
            destination_port: 1024,
        };
        assert!(tx.send(conn).is_ok());
    }
}
