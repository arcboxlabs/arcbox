//! In-band half-close and flow control over a stream that offers neither.
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
//! The same connection must also never be left unread. On
//! Virtualization.framework one guest→host stream the host stops draining
//! stalls every new vsock connection to the VM, so a `docker logs -f | less`
//! paused on the host wedged `docker version` for everyone. A stream's
//! backpressure therefore cannot be the socket: [`HalfCloseStream`] keeps a
//! task reading the connection at all times and lets each side send only
//! within a window ([`WINDOW`]) the other has granted, returned as the
//! consumer takes the bytes. A slow consumer stops the *sender* — dockerd
//! or the Docker client on its own socket — while the vsock stays drained.
//!
//! Every write goes out as a length-prefixed frame; shutting down the write
//! half sends a zero-length frame the peer reads as EOF while the fd stays
//! open for the other direction; a frame with [`GRANT`] set carries window
//! instead of bytes. Both ends of the Docker API vsock channel wrap their
//! stream in it, so the framing is invisible to the HTTP traffic above. A
//! peer that closes its fd outright is still EOF: an fd that ends between
//! frames reads as a clean end of stream.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf, WriteHalf};
use tokio::sync::mpsc;

use super::flow::Credit;

/// Largest payload one frame carries. Longer writes are split; a header
/// announcing more than this is a protocol error, which bounds what a
/// misbehaving peer can make the reader allocate.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// Payload bytes each side may have in flight towards the other: the window
/// a fresh stream holds open in each direction, and the most one side ever
/// buffers for a consumer that has stopped reading.
pub const WINDOW: usize = 1024 * 1024;

/// Header bit marking a window grant: the low bits are the bytes granted,
/// not a payload length.
const GRANT: u32 = 1 << 31;

const HEADER_LEN: usize = 4;

/// What the reading task hands the consumer.
enum Incoming {
    Data(Bytes),
    Eof,
    Failed(io::Error),
}

/// A byte stream whose half-close and flow control travel in-band.
///
/// Wire format: `[u32 BE header][payload]`, repeated. A header below
/// `GRANT` is a payload length, zero being this side's EOF, sent exactly
/// once by `poll_shutdown`; a header with `GRANT` set grants the peer that
/// many more payload bytes. The inner stream is never shut down, only
/// dropped.
#[derive(Debug)]
pub struct HalfCloseStream<T> {
    writer: WriteHalf<T>,
    /// Encoded frames not yet accepted by the writer. Holds at most one data
    /// frame plus pending grant and EOF markers.
    outgoing: BytesMut,
    /// What the peer lets this side send.
    send: Arc<Credit>,
    /// What this side lets the peer send; the reading task takes from it,
    /// the consumer gives back through `unreturned`.
    recv: Arc<Credit>,
    /// Bytes the consumer took that the peer has not been re-granted yet.
    unreturned: usize,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    /// Payload the consumer has not taken yet.
    current: Bytes,
    read_eof: bool,
    /// Our EOF frame has been queued; writes are refused from here on.
    write_closed: bool,
}

impl<T> HalfCloseStream<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    /// Wraps `inner`, starting the task that keeps it read. Must be called
    /// on a tokio runtime.
    pub fn new(inner: T) -> Self {
        let (reader, writer) = tokio::io::split(inner);
        let send = Arc::new(Credit::new(WINDOW));
        let recv = Arc::new(Credit::new(WINDOW));
        let (tx, incoming) = mpsc::unbounded_channel();
        tokio::spawn(read_frames(
            reader,
            tx,
            Arc::clone(&send),
            Arc::clone(&recv),
        ));
        Self {
            writer,
            outgoing: BytesMut::new(),
            send,
            recv,
            unreturned: 0,
            incoming,
            current: Bytes::new(),
            read_eof: false,
            write_closed: false,
        }
    }

    /// Pushes queued frame bytes into the writer until it stops accepting.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.outgoing.is_empty() {
            let n = ready!(Pin::new(&mut self.writer).poll_write(cx, &self.outgoing))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.outgoing.advance(n);
        }
        Poll::Ready(Ok(()))
    }

    /// Accounts for `n` payload bytes handed to the consumer, and once half
    /// the window has been taken, grants it back so the peer never waits on
    /// a consumer that keeps up.
    fn consumed(&mut self, n: usize) -> io::Result<()> {
        self.unreturned += n;
        if self.unreturned < WINDOW / 2 {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.unreturned);
        // Before the grant goes out, so bytes sent against it are in window.
        self.recv.grant(bytes)?;
        self.outgoing.put_u32(GRANT | bytes as u32);
        Ok(())
    }
}

/// Reads the connection until the peer closes it or the consumer is gone:
/// grants reopen the send window, payloads go to the consumer within the
/// window this side granted, and a zero-length frame is the peer's EOF —
/// after which grants may still arrive for this side's writes. Once it
/// stops, no grant can come, so a writer waiting for one fails instead of
/// waiting forever: a write-only proxy would otherwise never notice its
/// peer left.
async fn read_frames<R: AsyncRead + Unpin>(
    mut reader: R,
    out: mpsc::UnboundedSender<Incoming>,
    send: Arc<Credit>,
    recv: Arc<Credit>,
) {
    let result = read_frames_inner(&mut reader, &out, &send, &recv).await;
    send.close();
    let Some(result) = result else {
        return;
    };
    // A closed fd is EOF unless the peer already said so; anything else the
    // consumer learns on its next read.
    let _ = out.send(match result {
        Ok(true) => return,
        Ok(false) => Incoming::Eof,
        Err(e) => Incoming::Failed(e),
    });
}

/// The frame loop; `None` when the consumer went away, else whether the
/// peer's EOF frame was seen before its fd closed.
async fn read_frames_inner<R: AsyncRead + Unpin>(
    reader: &mut R,
    out: &mpsc::UnboundedSender<Incoming>,
    send: &Credit,
    recv: &Credit,
) -> Option<io::Result<bool>> {
    let mut eof = false;
    let result = loop {
        let mut header = [0u8; HEADER_LEN];
        let read = tokio::select! {
            read = reader.read_exact(&mut header) => read,
            () = out.closed() => return None,
        };
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break Ok(eof),
            Err(e) => break Err(e),
        }
        let header = u32::from_be_bytes(header);
        if header & GRANT != 0 {
            if let Err(e) = send.grant((header & !GRANT) as usize) {
                break Err(e);
            }
            continue;
        }
        let len = header as usize;
        if len == 0 {
            if eof {
                break Err(protocol_error("peer sent a second EOF frame"));
            }
            eof = true;
            let _ = out.send(Incoming::Eof);
            continue;
        }
        if eof {
            break Err(protocol_error("peer sent data after its EOF frame"));
        }
        if len > MAX_FRAME_PAYLOAD {
            break Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame payload of {len} bytes exceeds {MAX_FRAME_PAYLOAD}"),
            ));
        }
        if let Err(e) = recv.take(len) {
            break Err(e);
        }
        let mut payload = BytesMut::zeroed(len);
        let read = tokio::select! {
            read = reader.read_exact(&mut payload) => read,
            () = out.closed() => return None,
        };
        if let Err(e) = read {
            break Err(if e.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(e.kind(), "peer closed inside a frame payload")
            } else {
                e
            });
        }
        if out.send(Incoming::Data(payload.freeze())).is_err() {
            return None;
        }
    };
    Some(result)
}

fn protocol_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl<T> AsyncRead for HalfCloseStream<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // A writer that does not flush before turning to read (write_all
        // then read) must not deadlock on bytes parked in `outgoing`; the
        // same drain puts queued grants on the wire. A failed write is the
        // writer's to report: a peer that has closed its fd can no longer
        // take grants, while its last frames and EOF are still to be read.
        let _ = this.poll_drain(cx);
        if this.read_eof || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        while this.current.is_empty() {
            match ready!(this.incoming.poll_recv(cx)) {
                Some(Incoming::Data(bytes)) => this.current = bytes,
                Some(Incoming::Eof) => {
                    this.read_eof = true;
                    return Poll::Ready(Ok(()));
                }
                Some(Incoming::Failed(e)) => return Poll::Ready(Err(e)),
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream reader task ended",
                    )));
                }
            }
        }
        let n = this.current.len().min(buf.remaining());
        this.consumed(n)?;
        buf.put_slice(&this.current.split_to(n));
        // Best effort: the grant leaves now if the writer takes it, else on
        // the next read or write.
        let _ = this.poll_drain(cx);
        Poll::Ready(Ok(()))
    }
}

impl<T> AsyncWrite for HalfCloseStream<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
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
        // And never past the window the peer holds open.
        let len = ready!(this.send.poll_take(cx, data.len().min(MAX_FRAME_PAYLOAD)))?;
        this.outgoing.reserve(HEADER_LEN + len);
        this.outgoing.put_u32(len as u32);
        this.outgoing.extend_from_slice(&data[..len]);
        // Best effort push now; whatever the writer does not take waits for
        // the next drain, and the waker it registered covers that.
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.writer).poll_flush(cx)
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
        Pin::new(&mut this.writer).poll_flush(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt as _, DuplexStream};

    fn pair() -> (HalfCloseStream<DuplexStream>, HalfCloseStream<DuplexStream>) {
        let (a, b) = tokio::io::duplex(4096);
        (HalfCloseStream::new(a), HalfCloseStream::new(b))
    }

    /// A raw peer speaking the wire format by hand.
    fn raw_pair() -> (DuplexStream, HalfCloseStream<DuplexStream>) {
        let (raw, framed) = tokio::io::duplex(64);
        (raw, HalfCloseStream::new(framed))
    }

    async fn is_pending<F: Future>(fut: F) -> bool {
        tokio::time::timeout(Duration::from_millis(20), fut)
            .await
            .is_err()
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
        assert!(
            is_pending(a.read(&mut reply)).await,
            "a must not see EOF before b shuts down"
        );
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

    /// The other point: a consumer that stops reading stops the *writer*
    /// after one window, while the connection itself stays drained — and
    /// reading again lets the writer continue.
    #[tokio::test]
    async fn a_stalled_consumer_stops_the_writer_at_the_window_not_the_socket() {
        let (mut a, mut b) = pair();
        let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            let chunk = vec![7u8; MAX_FRAME_PAYLOAD];
            let mut sent = 0;
            while sent < WINDOW {
                a.write_all(&chunk).await.unwrap();
                sent += chunk.len();
            }
            // Window exhausted: the next write must wait for b to read.
            let more = tokio::time::timeout(Duration::from_millis(50), a.write_all(&chunk)).await;
            blocked_tx.send(more.is_err()).unwrap();
            a.write_all(&chunk).await.unwrap();
            a.shutdown().await.unwrap();
        });
        assert!(blocked_rx.await.unwrap(), "writer must block at the window");

        // Nothing was read, yet the whole window has left a's socket: the
        // reader task drained it. The 4 KiB duplex could not hold it.
        let mut got = Vec::new();
        b.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        // The timed-out write took no window, so it sent nothing.
        assert_eq!(got.len(), WINDOW + MAX_FRAME_PAYLOAD);
    }

    /// A proxy that only writes (dockerd output towards a host that left)
    /// must learn the peer is gone: the window it waits for will never
    /// reopen, so the write fails rather than hanging the session forever.
    #[tokio::test]
    async fn a_writer_blocked_on_the_window_fails_when_the_peer_drops_its_fd() {
        let (mut a, b) = pair();
        let chunk = vec![7u8; MAX_FRAME_PAYLOAD];
        for _ in 0..(WINDOW / MAX_FRAME_PAYLOAD) {
            a.write_all(&chunk).await.unwrap();
        }
        let blocked = tokio::spawn(async move {
            let err = a.write_all(&chunk).await.unwrap_err();
            err.kind()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(b);
        assert_eq!(blocked.await.unwrap(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn grants_still_arrive_after_the_peer_sent_eof() {
        let (mut a, mut b) = pair();
        b.shutdown().await.unwrap();
        let mut probe = [0u8; 1];
        assert_eq!(a.read(&mut probe).await.unwrap(), 0);

        let writer = tokio::spawn(async move {
            let chunk = vec![1u8; MAX_FRAME_PAYLOAD];
            for _ in 0..(2 * WINDOW / MAX_FRAME_PAYLOAD) {
                a.write_all(&chunk).await.unwrap();
            }
            a.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        b.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got.len(), 2 * WINDOW);
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
        let (mut raw, mut framed) = raw_pair();
        raw.write_all(&(MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 8];
        let err = framed.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Peer vanishes in the middle of a payload.
        let (mut raw, mut framed) = raw_pair();
        raw.write_all(&8u32.to_be_bytes()).await.unwrap();
        raw.write_all(b"abc").await.unwrap();
        drop(raw);
        let mut got = Vec::new();
        let err = framed.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_peer_overrunning_the_window_is_an_error() {
        // Room for the whole overrun frame, so the feeder finishes without
        // the reader task's help and the probe below cannot race it.
        let (mut raw, framed) = tokio::io::duplex(WINDOW + 2 * MAX_FRAME_PAYLOAD);
        let mut framed = HalfCloseStream::new(framed);
        let frames = WINDOW / MAX_FRAME_PAYLOAD + 1;
        let payload = vec![0u8; MAX_FRAME_PAYLOAD];
        for _ in 0..frames {
            raw.write_all(&(MAX_FRAME_PAYLOAD as u32).to_be_bytes())
                .await
                .unwrap();
            raw.write_all(&payload).await.unwrap();
        }
        // Nothing is consumed, so no window goes back: the whole window
        // arrives and the extra frame's header overruns it.
        let mut got = vec![0u8; WINDOW];
        framed.read_exact(&mut got).await.unwrap();
        let mut probe = [0u8; 1];
        let err = framed.read(&mut probe).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_peer_granting_more_window_than_exists_is_an_error() {
        let (mut raw, mut framed) = raw_pair();
        raw.write_all(&(GRANT | 1).to_be_bytes()).await.unwrap();
        let mut probe = [0u8; 1];
        let err = framed.read(&mut probe).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
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
