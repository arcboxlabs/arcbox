//! Blocking transport for connected host-guest stream sockets.
//!
//! Uses `std::os::unix::net::UnixStream` with `libc::poll`-based deadlines.
//! No tokio dependency — safe to use from any thread without risking reactor
//! or timer driver interference.
//!
//! Wire format is identical to [`super::VsockTransport`]: 4-byte BE length prefix
//! followed by payload. Messages produced by `AgentClient::build_message`
//! work unchanged.

use bytes::Bytes;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use crate::error::{Result, TransportError};

/// Maximum frame size (must match async transport).
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Blocking transport over an AF_VSOCK or AF_UNIX stream socket.
///
/// All I/O uses `libc::poll` for deadline enforcement. The underlying fd
/// is set to non-blocking; `poll` + `read`/`write` loops handle partial
/// transfers and `EAGAIN`.
pub struct BlockingVsockTransport {
    stream: Option<UnixStream>,
}

/// Sole writer of a split blocking transport, with a deadline for each send.
pub struct BlockingVsockSender(UnixStream);

impl BlockingVsockSender {
    /// Sends bytes that already include the frame's length prefix.
    pub fn send(&mut self, data: &[u8], deadline: Instant) -> Result<()> {
        BlockingVsockTransport::send_to(&mut self.0, data, deadline)
    }
}

/// Sole reader of a split blocking transport, with no idle deadline.
pub struct BlockingVsockReceiver(UnixStream);

impl BlockingVsockReceiver {
    /// Receives one full frame, including its length prefix.
    ///
    /// Retain a shutdown handle to cancel an idle or incomplete read.
    pub fn recv(&mut self) -> Result<Bytes> {
        BlockingVsockTransport::recv_from(&mut self.0, None)
    }
}

/// Cancels pending I/O on a blocking transport when the owner drops this handle.
/// The duplicate socket keeps the file descriptor valid until cancellation.
pub struct BlockingVsockShutdown(UnixStream);

impl Drop for BlockingVsockShutdown {
    fn drop(&mut self) {
        // An already-disconnected socket needs no further cleanup.
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

impl BlockingVsockTransport {
    /// Returns a handle whose drop interrupts blocking send/receive operations.
    pub fn shutdown_handle(&self) -> io::Result<BlockingVsockShutdown> {
        self.stream
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?
            .try_clone()
            .map(BlockingVsockShutdown)
    }

    /// Closes the owned stream. A closed transport cannot reconnect.
    pub fn close(&mut self) {
        self.stream.take();
    }

    /// Consumes the transport into independent blocking send and receive halves.
    ///
    /// Obtain a shutdown handle before splitting; cancellation must interrupt and join both workers.
    ///
    /// # Errors
    /// Returns an error if the transport is closed or the socket cannot be duplicated.
    pub fn into_split(self) -> Result<(BlockingVsockSender, BlockingVsockReceiver)> {
        let reader = self.stream.ok_or(TransportError::NotConnected)?;
        let writer = reader.try_clone()?;
        Ok((BlockingVsockSender(writer), BlockingVsockReceiver(reader)))
    }

    /// Creates a transport from a raw fd (takes ownership).
    ///
    /// # Safety
    /// `fd` must be an owned, connected AF_VSOCK or AF_UNIX stream socket fd.
    pub unsafe fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
        // SAFETY: the caller transfers ownership of a connected stream socket.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream: Some(stream),
        })
    }

    /// Sends a framed message with a deadline.
    ///
    /// `data` must already include the 4-byte length prefix (as produced by
    /// `AgentClient::build_message`).
    pub fn send(&mut self, data: &[u8], deadline: Instant) -> Result<()> {
        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;
        Self::send_to(stream, data, deadline)
    }

    /// Receives a framed message with a deadline.
    ///
    /// Returns the full frame (4-byte length prefix + payload), matching
    /// the async transport's `read_framed` semantics.
    pub fn recv(&mut self, deadline: Instant) -> Result<Bytes> {
        let stream = self.stream.as_mut().ok_or(TransportError::NotConnected)?;
        Self::recv_from(stream, Some(deadline))
    }

    fn send_to(stream: &mut UnixStream, data: &[u8], deadline: Instant) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            Self::poll_ready(stream, libc::POLLOUT, Some(deadline))?;
            match stream.write(&data[offset..]) {
                Ok(0) => {
                    return Err(TransportError::io(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned 0",
                    )));
                }
                Ok(n) => offset += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(TransportError::io(e)),
            }
        }
        Ok(())
    }

    fn recv_from(stream: &mut UnixStream, deadline: Option<Instant>) -> Result<Bytes> {
        // Read 4-byte length prefix.
        let mut len_buf = [0u8; 4];
        Self::read_exact(stream, &mut len_buf, deadline)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(TransportError::Protocol(format!(
                "frame too large: {len} bytes (max {MAX_FRAME_SIZE})"
            )));
        }

        // Read payload.
        let mut buf = vec![0u8; 4 + len];
        buf[..4].copy_from_slice(&len_buf);
        Self::read_exact(stream, &mut buf[4..], deadline)?;
        Ok(Bytes::from(buf))
    }

    fn read_exact(
        stream: &mut UnixStream,
        buf: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            Self::poll_ready(stream, libc::POLLIN, deadline)?;
            match stream.read(&mut buf[offset..]) {
                Ok(0) => {
                    return Err(TransportError::io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "early eof",
                    )));
                }
                Ok(n) => offset += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(TransportError::io(e)),
            }
        }
        Ok(())
    }

    fn poll_ready(stream: &UnixStream, events: i16, deadline: Option<Instant>) -> Result<()> {
        let timeout_ms = if let Some(deadline) = deadline {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    TransportError::io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "transport deadline exceeded",
                    ))
                })?;
            remaining.as_millis().min(i32::MAX as u128) as i32
        } else {
            -1
        };

        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: pfd is valid, initialized, and points to our owned fd.
        let ret = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                // EINTR — caller will retry.
                return Ok(());
            }
            return Err(TransportError::io(err));
        }
        if ret == 0 {
            return Err(TransportError::io(io::Error::new(
                io::ErrorKind::TimedOut,
                "transport deadline exceeded",
            )));
        }
        // POLLNVAL means the fd itself is invalid -- return an error
        // immediately rather than letting a subsequent read/write surface it.
        if pfd.revents & libc::POLLNVAL != 0 {
            return Err(TransportError::io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "poll: POLLNVAL -- file descriptor is not valid",
            )));
        }
        // POLLERR/POLLHUP: the fd is valid but the peer hung up or an error
        // occurred. Fall through — the subsequent read/write will surface the
        // actual error.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::time::Duration;

    #[test]
    fn closed_transport_rejects_io_and_releases_the_socket() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        // SAFETY: the transport receives ownership of a connected stream socket.
        let mut client =
            unsafe { BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap() };
        client.close();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(matches!(
            client.send(&[], deadline),
            Err(TransportError::NotConnected)
        ));
        assert!(matches!(
            client.recv(deadline),
            Err(TransportError::NotConnected)
        ));
        assert_eq!(
            client.shutdown_handle().err().unwrap().kind(),
            io::ErrorKind::NotConnected
        );
        assert!(matches!(
            client.into_split(),
            Err(TransportError::NotConnected)
        ));
        assert_eq!(guest.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn split_keeps_receiving_while_the_writer_is_idle() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // SAFETY: host transfers ownership of its connected socket.
        let transport = unsafe { BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap() };
        let _shutdown = transport.shutdown_handle().unwrap();
        let (mut writer, mut reader) = transport.into_split().unwrap();
        let (started, ready) = std::sync::mpsc::channel();
        let (received, events) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            received.send(reader.recv()).unwrap();
            received.send(reader.recv()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let frame = [0, 0, 0, 2, b'o', b'k'];
        writer
            .send(&frame, Instant::now() + Duration::from_secs(2))
            .unwrap();
        let mut request = [0; 6];
        guest.read_exact(&mut request).unwrap();
        assert_eq!(request, frame);
        guest.write_all(&frame).unwrap();
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .as_ref(),
            frame
        );
        drop(guest);
        assert!(
            events
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_releases_an_idle_reader_and_a_saturated_writer() {
        let (mut host, _guest) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        loop {
            match host.write(&[0; 8192]) {
                Ok(written) => assert!(written > 0),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("cannot fill socket: {error}"),
            }
        }
        // SAFETY: host transfers ownership of its connected socket.
        let transport = unsafe { BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap() };
        let shutdown = transport.shutdown_handle().unwrap();
        let (mut writer, mut reader) = transport.into_split().unwrap();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, results) = std::sync::mpsc::channel();
        let read_worker = std::thread::spawn({
            let started = started.clone();
            let finished = finished.clone();
            move || {
                started.send(()).unwrap();
                finished.send(reader.recv().map(|_| ())).unwrap();
            }
        });
        let write_worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(writer.send(&[1], Instant::now() + Duration::from_secs(5)))
                .unwrap();
        });
        for _ in 0..2 {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        drop(shutdown);
        for _ in 0..2 {
            let error = results
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err();
            assert!(!error.to_string().contains("deadline"), "{error}");
        }
        read_worker.join().unwrap();
        write_worker.join().unwrap();
    }

    #[test]
    fn split_receiver_rejects_oversized_and_incomplete_frames() {
        for (frame, oversized) in [
            (u32::MAX.to_be_bytes().to_vec(), true),
            (vec![0, 0, 0, 2, b'x'], false),
        ] {
            let (host, mut guest) = UnixStream::pair().unwrap();
            // SAFETY: host transfers ownership of its connected socket.
            let transport =
                unsafe { BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap() };
            let (_writer, mut reader) = transport.into_split().unwrap();
            guest.write_all(&frame).unwrap();
            drop(guest);
            let error = reader.recv().unwrap_err();
            assert_eq!(
                matches!(error, TransportError::Protocol(_)),
                oversized,
                "{error}"
            );
        }
    }

    #[test]
    fn test_blocking_roundtrip() {
        let mut fds: [libc::c_int; 2] = [0; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        let mut client = unsafe { BlockingVsockTransport::from_raw_fd(fds[0]).unwrap() };
        let mut server = unsafe { BlockingVsockTransport::from_raw_fd(fds[1]).unwrap() };

        let deadline = Instant::now() + Duration::from_secs(2);

        // Build a framed message: 4-byte BE length + payload.
        let payload = b"hello";
        let len = (payload.len() as u32).to_be_bytes();
        let mut frame = Vec::new();
        frame.extend_from_slice(&len);
        frame.extend_from_slice(payload);

        client.send(&frame, deadline).unwrap();
        let received = server.recv(deadline).unwrap();
        assert_eq!(&received[4..], b"hello");
    }

    #[test]
    fn test_blocking_timeout() {
        let mut fds: [libc::c_int; 2] = [0; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        let mut client = unsafe { BlockingVsockTransport::from_raw_fd(fds[0]).unwrap() };
        // Don't write anything on fds[1] — recv should timeout.
        let _server_fd = unsafe { UnixStream::from_raw_fd(fds[1]) };

        let deadline = Instant::now() + Duration::from_millis(50);
        let result = client.recv(deadline);
        assert!(result.is_err());
    }

    #[test]
    fn test_blocking_peer_close_eof() {
        let mut fds: [libc::c_int; 2] = [0; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        let mut client = unsafe { BlockingVsockTransport::from_raw_fd(fds[0]).unwrap() };
        // Close peer immediately.
        unsafe { libc::close(fds[1]) };

        let deadline = Instant::now() + Duration::from_secs(2);
        let result = client.recv(deadline);
        assert!(result.is_err());
    }
}
