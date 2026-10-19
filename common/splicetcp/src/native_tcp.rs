//! Native socket ownership and cancellation across TCP bridge handoffs.

use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;

/// An opaque socket-owner lease and a cancellation signal.
///
/// Cancellation observers must retain only the token. A strong lease belongs
/// to a native socket owner and can delay drain until that socket is dropped.
struct TcpLifecycle {
    keepalive: Arc<dyn Send + Sync>,
    cancelled: CancellationToken,
}

/// A native socket whose lifecycle survives conversion and descriptor cloning.
///
/// The socket field must precede the lifecycle field: closing the socket must
/// happen before the final lease can report that all socket owners are gone.
pub struct NativeTcp<S> {
    stream: S,
    lifecycle: Option<TcpLifecycle>,
}

impl<S> NativeTcp<S> {
    /// Wrap a native socket without managed lifecycle state.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            lifecycle: None,
        }
    }

    /// Attach an owner lease and a cancellation signal to a native socket.
    pub fn with_lifecycle(
        stream: S,
        keepalive: Arc<dyn Send + Sync>,
        cancelled: CancellationToken,
    ) -> Self {
        Self {
            stream,
            lifecycle: Some(TcpLifecycle {
                keepalive,
                cancelled,
            }),
        }
    }

    /// Return a cancellation observer without retaining the socket-owner lease.
    pub fn cancellation(&self) -> Option<CancellationToken> {
        self.lifecycle.as_ref().map(|state| state.cancelled.clone())
    }

    /// Return whether the socket's managed lifecycle has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.lifecycle
            .as_ref()
            .is_some_and(|state| state.cancelled.is_cancelled())
    }

    /// Extract a socket only when no managed owner lease would be discarded.
    ///
    /// A rejected managed socket is closed before its lease is released.
    pub fn into_unmanaged(self) -> io::Result<S> {
        if self.lifecycle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this native TCP consumer does not preserve managed socket ownership",
            ));
        }
        Ok(self.stream)
    }
}

impl<S> From<S> for NativeTcp<S> {
    fn from(stream: S) -> Self {
        Self::new(stream)
    }
}

impl NativeTcp<tokio::net::TcpStream> {
    /// Deregister the socket from Tokio without releasing its owner lease.
    pub fn into_std(self) -> io::Result<NativeTcp<std::net::TcpStream>> {
        let Self { stream, lifecycle } = self;
        let stream = stream.into_std()?;
        Ok(NativeTcp { stream, lifecycle })
    }
}

impl NativeTcp<std::net::TcpStream> {
    /// Register the socket with Tokio without releasing its owner lease.
    pub fn into_tokio(self) -> io::Result<NativeTcp<tokio::net::TcpStream>> {
        let Self { stream, lifecycle } = self;
        let stream = tokio::net::TcpStream::from_std(stream)?;
        Ok(NativeTcp { stream, lifecycle })
    }

    /// Duplicate the native descriptor and retain its socket-owner lease.
    pub fn try_clone(&self) -> io::Result<Self> {
        let stream = self.stream.try_clone()?;
        let lifecycle = self.lifecycle.as_ref().map(|state| TcpLifecycle {
            keepalive: Arc::clone(&state.keepalive),
            cancelled: state.cancelled.clone(),
        });
        Ok(Self { stream, lifecycle })
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.stream.set_nonblocking(nonblocking)
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.stream.set_nodelay(nodelay)
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.stream.shutdown(how)
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.peek(buf)
    }
}

#[cfg(unix)]
impl<S: std::os::fd::AsFd> std::os::fd::AsFd for NativeTcp<S> {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

#[cfg(windows)]
impl<S: std::os::windows::io::AsSocket> std::os::windows::io::AsSocket for NativeTcp<S> {
    fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
        self.stream.as_socket()
    }
}

impl<S: Read> Read for NativeTcp<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.stream.read_vectored(bufs)
    }
}

impl<S: Write> Write for NativeTcp<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.stream.write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for NativeTcp<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for NativeTcp<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
