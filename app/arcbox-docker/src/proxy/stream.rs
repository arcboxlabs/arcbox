//! Async I/O wrapper around a raw file descriptor.
//!
//! Provides [`RawFdStream`], a tokio-compatible async stream that wraps a
//! blocking fd with non-blocking I/O via [`AsyncFd`]. Used by all guest
//! proxy paths as the underlying transport.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct RawFdWrapper(OwnedFd);

impl AsRawFd for RawFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub struct RawFdStream {
    inner: AsyncFd<RawFdWrapper>,
}

impl RawFdStream {
    /// Creates a new async stream from an owned file descriptor.
    ///
    /// The fd is set to non-blocking mode. Ownership is taken via
    /// [`OwnedFd`] so the descriptor is always closed on error.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        Self::set_nonblocking(fd.as_raw_fd())?;
        let inner = AsyncFd::new(RawFdWrapper(fd))?;
        Ok(Self { inner })
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: F_GETFL/F_SETFL are safe on valid fds.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsyncRead for RawFdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let slice = buf.initialize_unfilled();
                // SAFETY: reading into our buffer from a valid fd.
                let n = unsafe { libc::read(fd, slice.as_mut_ptr().cast(), slice.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // n >= 0 is enforced above.
                    #[allow(clippy::cast_sign_loss)]
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for RawFdStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                // SAFETY: writing from our buffer to a valid fd.
                let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // n >= 0 is enforced above.
                    #[allow(clippy::cast_sign_loss)]
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // macOS vsock does not support half-close: `shutdown(fd, SHUT_WR)`
        // destroys the entire connection instead of closing only the write
        // side. This breaks hyper's upgrade path because the fd becomes
        // ENOTCONN before the bridge can start. The fd is properly closed
        // when `OwnedFd` is dropped, so the no-op is safe.
        #[cfg(target_os = "macos")]
        {
            Poll::Ready(Ok(()))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let fd = self.inner.get_ref().as_raw_fd();
            // SAFETY: shutdown on a valid fd.
            let result = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
            if result == 0 {
                return Poll::Ready(Ok(()));
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOTCONN) {
                return Poll::Ready(Ok(()));
            }
            Poll::Ready(Err(err))
        }
    }
}
