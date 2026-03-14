use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
        // SAFETY: fd is a valid open file descriptor; command and arguments are valid.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is valid; O_NONBLOCK is a safe flag to set.
        // SAFETY: fd is a valid open file descriptor; command and arguments are valid.
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
        // SAFETY: fd is a valid open file descriptor with exclusive ownership.
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
                // SAFETY: fd is a valid open socket file descriptor.
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
