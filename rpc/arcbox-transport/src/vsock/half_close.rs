//! In-band half-close over a stream that cannot half-close by itself.
//!
//! A Docker attach or exec session needs its two directions to close
//! independently: the CLI sends stdin and half-closes its side, and the
//! container keeps writing until it exits. Between host and guest that
//! session rides a vsock connection, and neither macOS backend turns a
//! `shutdown(SHUT_WR)` on that fd into anything the other side observes —
//! Virtualization.framework hands out a socket whose half-close is silent,
//! and the custom HV device maps only a *guest* half-close onto the host fd,
//! never the reverse. A `docker run -i` fed from a pipe therefore never
//! delivered stdin EOF to the container and hung once the pipe closed
//! (arcboxlabs/arcbox#268).
//!
//! [`HalfCloseStream`] carries EOF in-band instead. Every write goes out as
//! a length-prefixed frame, and shutting down the write half sends a
//! zero-length frame that the peer's reader reports as EOF while the fd
//! stays open for the other direction. Both ends of the Docker API vsock
//! channel wrap their stream in it, so the framing is invisible to the HTTP
//! traffic above. A peer that closes its fd outright is still EOF: an fd
//! that ends between frames reads as a clean end of stream.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Largest payload one frame carries. Longer writes are split; a header
/// announcing more than this is a protocol error, which bounds what a
/// misbehaving peer can make the reader allocate.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

const HEADER_LEN: usize = 4;

/// A byte stream whose half-close travels in-band as a zero-length frame.
///
/// Wire format: `[u32 BE payload length][payload]`, repeated; a length of
/// zero is this side's EOF and is sent exactly once, by `poll_shutdown`.
/// The inner stream is never shut down, only dropped.
#[derive(Debug)]
pub struct HalfCloseStream<T> {
    inner: T,
    /// Encoded frames not yet accepted by `inner`. Holds at most one frame
    /// plus a pending EOF marker, which is what gives writers backpressure.
    outgoing: BytesMut,
    /// Header of the frame currently being read, filled incrementally.
    header: [u8; HEADER_LEN],
    header_filled: usize,
    /// Payload bytes of the current incoming frame still to hand out.
    incoming_remaining: usize,
    /// The peer sent its EOF frame or closed its fd.
    read_eof: bool,
    /// Our EOF frame has been queued; writes are refused from here on.
    write_closed: bool,
}

impl<T> HalfCloseStream<T> {
    /// Wraps `inner`.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            outgoing: BytesMut::new(),
            header: [0; HEADER_LEN],
            header_filled: 0,
            incoming_remaining: 0,
            read_eof: false,
            write_closed: false,
        }
    }

    /// The wrapped stream.
    pub fn get_ref(&self) -> &T {
        &self.inner
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> HalfCloseStream<T> {
    /// Pushes queued frame bytes into `inner` until it stops accepting.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.outgoing.is_empty() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.outgoing))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.outgoing.advance(n);
        }
        Poll::Ready(Ok(()))
    }

    /// Reads the next frame header, returning its payload length. `None`
    /// means the peer closed its fd cleanly between frames.
    fn poll_header(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<usize>>> {
        while self.header_filled < HEADER_LEN {
            let mut header = ReadBuf::new(&mut self.header[self.header_filled..]);
            ready!(Pin::new(&mut self.inner).poll_read(cx, &mut header))?;
            let n = header.filled().len();
            if n == 0 {
                if self.header_filled == 0 {
                    return Poll::Ready(Ok(None));
                }
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed inside a frame header",
                )));
            }
            self.header_filled += n;
        }
        self.header_filled = 0;
        let len = u32::from_be_bytes(self.header) as usize;
        if len > MAX_FRAME_PAYLOAD {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame payload of {len} bytes exceeds {MAX_FRAME_PAYLOAD}"),
            )));
        }
        Poll::Ready(Ok(Some(len)))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for HalfCloseStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // A writer that does not flush before turning to read (write_all
        // then read) must not deadlock on bytes parked in `outgoing`.
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        if this.read_eof || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.incoming_remaining == 0 {
            match ready!(this.poll_header(cx))? {
                None | Some(0) => {
                    this.read_eof = true;
                    return Poll::Ready(Ok(()));
                }
                Some(len) => this.incoming_remaining = len,
            }
        }

        let want = this.incoming_remaining.min(buf.remaining());
        let target = &mut buf.initialize_unfilled_to(want)[..want];
        let mut payload = ReadBuf::new(target);
        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut payload))?;
        let n = payload.filled().len();
        if n == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed inside a frame payload",
            )));
        }
        buf.advance(n);
        this.incoming_remaining -= n;
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for HalfCloseStream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.write_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write half already shut down",
            )));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // One frame in flight at a time: the previous one must be on the
        // wire before the next is accepted, so a slow peer slows the writer.
        ready!(this.poll_drain(cx))?;

        let len = data.len().min(MAX_FRAME_PAYLOAD);
        this.outgoing.reserve(HEADER_LEN + len);
        this.outgoing.put_u32(len as u32);
        this.outgoing.extend_from_slice(&data[..len]);
        // Best effort push now; whatever `inner` does not take waits for the
        // next drain, and the waker `inner` registered covers that.
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    /// Sends the EOF frame. The inner stream is deliberately left open:
    /// the peer may still be writing to us.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if !this.write_closed {
            ready!(this.poll_drain(cx))?;
            this.outgoing.put_u32(0);
            this.write_closed = true;
        }
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn pair() -> (
        HalfCloseStream<tokio::io::DuplexStream>,
        HalfCloseStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(4096);
        (HalfCloseStream::new(a), HalfCloseStream::new(b))
    }

    #[tokio::test]
    async fn bytes_round_trip_in_both_directions() {
        let (mut a, mut b) = pair();
        a.write_all(b"to-b").await.unwrap();
        b.write_all(b"to-a").await.unwrap();

        let mut got = [0u8; 4];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"to-b");
        a.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"to-a");
    }

    /// The whole point: one side's shutdown is the other side's EOF, and
    /// the reverse direction keeps working afterwards.
    #[tokio::test]
    async fn shutdown_is_eof_for_the_peer_but_leaves_the_other_direction_open() {
        let (mut a, mut b) = pair();
        a.write_all(b"last words").await.unwrap();
        a.shutdown().await.unwrap();

        let mut got = Vec::new();
        b.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"last words");

        // b → a still flows after a's half-close.
        b.write_all(b"reply").await.unwrap();
        let mut reply = [0u8; 5];
        a.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");

        // And a's reader is still open until b closes too.
        let pending = tokio::time::timeout(Duration::from_millis(20), a.read(&mut reply)).await;
        assert!(pending.is_err(), "a must not see EOF before b shuts down");
        b.shutdown().await.unwrap();
        assert_eq!(a.read(&mut reply).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn peer_dropping_its_fd_between_frames_is_a_clean_eof() {
        let (mut a, mut b) = pair();
        a.write_all(b"x").await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        let mut got = Vec::new();
        b.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"x");
    }

    #[tokio::test]
    async fn writes_larger_than_a_frame_are_split_and_reassembled() {
        let (mut a, mut b) = pair();
        let big: Vec<u8> = (0..(MAX_FRAME_PAYLOAD * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let expected = big.clone();

        let writer = tokio::spawn(async move {
            a.write_all(&big).await.unwrap();
            a.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        b.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn writes_after_shutdown_are_refused() {
        let (mut a, _b) = pair();
        a.shutdown().await.unwrap();
        let err = a.write_all(b"late").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn oversized_or_truncated_frames_are_errors_not_hangs() {
        // Header announcing more than the cap.
        let (mut raw, framed) = tokio::io::duplex(64);
        let mut framed = HalfCloseStream::new(framed);
        raw.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        let mut buf = [0u8; 8];
        let err = framed.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Peer vanishes in the middle of a payload.
        let (mut raw, framed) = tokio::io::duplex(64);
        let mut framed = HalfCloseStream::new(framed);
        raw.write_all(&8u32.to_be_bytes()).await.unwrap();
        raw.write_all(b"abc").await.unwrap();
        drop(raw);
        let mut got = Vec::new();
        let err = framed.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(got, b"abc", "bytes before the cut are still delivered");
    }

    /// `write_all` then `read` without a flush in between (the raw HTTP
    /// upgrade exchange does this) must still put the bytes on the wire.
    #[tokio::test]
    async fn read_drains_bytes_a_writer_forgot_to_flush() {
        let (mut a, mut b) = pair();
        let echo = tokio::spawn(async move {
            let mut got = [0u8; 4];
            b.read_exact(&mut got).await.unwrap();
            b.write_all(&got).await.unwrap();
            b.flush().await.unwrap();
        });
        a.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        a.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        echo.await.unwrap();
    }
}
