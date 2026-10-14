//! Async batch datagram I/O, integrating [`BatchDgram`] with tokio's
//! [`AsyncFd`] for readiness notification.
//!
//! Requires the `tokio` feature.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use tokio::io::unix::AsyncFd;

use crate::batch::{BatchDgram, RxDatagramV4, RxEntry};

/// Async batch datagram I/O backed by tokio's [`AsyncFd`].
///
/// Combines [`BatchDgram`] with readiness-based I/O polling. The FD is
/// set to `O_NONBLOCK` during construction.
pub struct AsyncBatchDgram {
    inner: AsyncFd<OwnedFd>,
    batch: BatchDgram,
}

impl AsyncBatchDgram {
    /// Creates a new async batch context, registering `fd` with the tokio
    /// reactor.
    ///
    /// Sets `O_NONBLOCK` on `fd` — required for the readiness-based polling
    /// model and to keep batch syscalls from blocking the reactor thread.
    ///
    /// # Errors
    ///
    /// Returns an error if `fcntl(F_GETFL|F_SETFL)` fails or the tokio
    /// reactor registration fails.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(fd.as_raw_fd())?;
        Ok(Self {
            inner: AsyncFd::new(fd)?,
            batch: BatchDgram::new(),
        })
    }

    /// Returns the underlying raw file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }

    /// Waits for the FD to become readable, then receives a batch.
    ///
    /// Returns a slice of [`RxEntry`] mirroring [`BatchDgram::recv_batch`].
    /// `entries.len()` is the number of datagrams received and
    /// `entries[i].len` is the byte length written into `bufs[i]`.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors from the underlying batch receive. `WouldBlock`
    /// is handled internally by clearing readiness and re-awaiting.
    #[allow(clippy::future_not_send)] // intentional: contains raw pointers from iovec arrays
    pub async fn recv_batch(&mut self, bufs: &mut [&mut [u8]]) -> io::Result<&[RxEntry]> {
        // Drive readiness in an inner scope that yields just the datagram
        // count; re-borrow entries from self.batch afterwards. The detour
        // avoids the NLL/Polonius limitation where returning a borrow from
        // a loop iteration prevents the next iteration's reborrow.
        let n = {
            let Self { inner, batch } = self;
            loop {
                let mut guard = inner.readable().await?;
                let fd = guard.get_inner().as_raw_fd();
                match batch.recv_batch(fd, bufs) {
                    Ok(entries) => break entries.len(),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        guard.clear_ready();
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        Ok(self.batch.last_entries(n))
    }

    /// Waits for IPv4 UDP datagrams and receives a prefix of caller-owned slots.
    ///
    /// Returns the datagram count, capped at [`BatchDgram::recv_capacity`]. Only
    /// the returned prefix has valid length and source metadata. Empty datagrams
    /// count as one; empty `slots` return zero without waiting for readiness.
    ///
    /// Each buffer must accommodate the largest datagram the caller accepts.
    /// Darwin silently truncates oversized datagrams without `MSG_TRUNC`.
    /// To reject datagrams above a size limit, provide at least one extra byte
    /// and reject a returned length above that limit.
    ///
    /// # Cancel safety
    ///
    /// Cancelling a pending receive does not consume datagrams. The method does
    /// not yield between a successful receive and returning its count.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`BatchDgram::recv_batch_from_v4`]. `WouldBlock`
    /// clears readiness and waits again. A non-IPv4 source returns `InvalidData`
    /// after consuming the batch.
    pub async fn recv_batch_from_v4(
        &mut self,
        slots: &mut [RxDatagramV4<'_>],
    ) -> io::Result<usize> {
        if slots.is_empty() {
            return Ok(0);
        }
        let Self { inner, batch } = self;
        loop {
            let mut guard = inner.readable().await?;
            match batch.recv_batch_from_v4(guard.get_inner().as_raw_fd(), slots) {
                Ok(count) => return Ok(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => guard.clear_ready(),
                Err(error) => return Err(error),
            }
        }
    }

    /// Sends `bufs` in full, looping over readiness as needed.
    ///
    /// Partial sends (where the kernel accepts only some datagrams before
    /// the buffer fills) are handled internally — this returns only once
    /// every datagram has been queued or an unrecoverable error is hit.
    /// `WouldBlock` causes the loop to clear readiness and re-await.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors from the underlying batch send other than
    /// `WouldBlock`. On error, any datagrams sent before the failure are
    /// already on the wire — there is no rollback.
    #[allow(clippy::future_not_send)] // intentional: contains raw pointers from iovec arrays
    pub async fn send_batch(&mut self, bufs: &[&[u8]]) -> io::Result<()> {
        let Self { inner, batch } = self;
        let mut sent = 0;
        while sent < bufs.len() {
            let mut guard = inner.writable().await?;
            let fd = guard.get_inner().as_raw_fd();
            match batch.send_batch(fd, &bufs[sent..]) {
                Ok(n) if n > 0 => sent += n,
                Ok(_) => guard.clear_ready(),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => guard.clear_ready(),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Provides read access to the inner [`AsyncFd`] for use in custom
    /// `tokio::select!` loops.
    pub fn inner(&self) -> &AsyncFd<OwnedFd> {
        &self.inner
    }
}

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

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, UdpSocket};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    /// Creates a socketpair with 1 MB buffers. `O_NONBLOCK` is set later
    /// by [`AsyncBatchDgram::new`].
    fn socketpair() -> (OwnedFd, OwnedFd) {
        let mut fds: [i32; 2] = [0; 2];
        // SAFETY: fds is a valid 2-element array; AF_UNIX SOCK_DGRAM is universally supported.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        // SAFETY: fds[0]/fds[1] are freshly allocated by socketpair, owned by us.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [a.as_raw_fd(), b.as_raw_fd()] {
            set_buf_size(fd, libc::SO_SNDBUF, 1024 * 1024);
            set_buf_size(fd, libc::SO_RCVBUF, 1024 * 1024);
        }
        (a, b)
    }

    fn set_buf_size(fd: RawFd, name: libc::c_int, size: libc::c_int) {
        // SAFETY: fd is valid for the duration of the setsockopt call.
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                std::ptr::from_ref(&size).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        assert_eq!(
            ret,
            0,
            "setsockopt(name={name}) failed for fd {fd}: {}",
            io::Error::last_os_error()
        );
    }

    #[tokio::test]
    async fn test_async_roundtrip() {
        let (a, b) = socketpair();

        let mut sender = AsyncBatchDgram::new(a).unwrap();
        let mut receiver = AsyncBatchDgram::new(b).unwrap();

        let payloads: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100]).collect();
        let bufs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
        sender.send_batch(&bufs).await.unwrap();

        let mut recv_buffers: Vec<Vec<u8>> = (0..5).map(|_| vec![0u8; 256]).collect();
        let mut recv_bufs: Vec<&mut [u8]> =
            recv_buffers.iter_mut().map(|b| b.as_mut_slice()).collect();
        let entries = receiver.recv_batch(&mut recv_bufs).await.unwrap();
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.len, 100);
            assert_eq!(recv_buffers[i][0], i as u8);
        }
    }

    #[tokio::test]
    async fn test_async_concurrent() {
        let (a, b) = socketpair();

        let mut sender = AsyncBatchDgram::new(a).unwrap();
        let mut receiver = AsyncBatchDgram::new(b).unwrap();

        let payloads: Vec<Vec<u8>> = (0..50u8).map(|i| vec![i; 64]).collect();
        let bufs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
        sender.send_batch(&bufs).await.unwrap();

        let mut total = 0;
        while total < 50 {
            let mut buffers: Vec<Vec<u8>> = (0..64).map(|_| vec![0u8; 128]).collect();
            let mut bufs: Vec<&mut [u8]> = buffers.iter_mut().map(|b| b.as_mut_slice()).collect();
            let entries = receiver.recv_batch(&mut bufs).await.unwrap();
            total += entries.len();
        }
        assert_eq!(total, 50);
    }

    #[tokio::test]
    async fn addressed_receive_preserves_sources_empty_datagrams_and_slot_limits() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut receiver = AsyncBatchDgram::new(socket.into()).unwrap();
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peers = [&first, &second, &first];
        let payloads: [&[u8]; 3] = [b"first", b"", b"longer-than-slot"];
        for (peer, payload) in peers.iter().zip(payloads) {
            assert_eq!(peer.send_to(payload, address).unwrap(), payload.len());
        }

        timeout(Duration::from_secs(2), async {
            let mut buffers = [[0xA5; 16]; 3];
            let mut offset = 0;
            while offset < payloads.len() {
                let mut chunks = buffers.iter_mut();
                let mut slots: [_; 3] =
                    std::array::from_fn(|_| RxDatagramV4::new(&mut chunks.next().unwrap()[..8]));
                let count = receiver
                    .recv_batch_from_v4(&mut slots[..payloads.len() - offset])
                    .await
                    .unwrap();
                assert!(count > 0);
                for (i, slot) in slots[..count].iter().enumerate() {
                    let expected = payloads[offset + i];
                    assert_eq!(slot.len, expected.len().min(8));
                    assert_eq!(&slot.buffer[..slot.len], &expected[..slot.len]);
                    assert_eq!(
                        SocketAddr::V4(slot.source),
                        peers[offset + i].local_addr().unwrap()
                    );
                }
                offset += count;
            }
            for buffer in buffers {
                assert_eq!(&buffer[8..], &[0xA5; 8]);
            }
        })
        .await
        .expect("IPv4 batch receive stalled");
    }

    #[tokio::test]
    async fn addressed_receive_cancels_while_idle_and_rearms_after_drain() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut receiver = AsyncBatchDgram::new(socket.into()).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), receiver.recv_batch_from_v4(&mut []))
                .await
                .expect("empty slots waited for readiness")
                .unwrap(),
            0
        );

        let mut buffer = [0; 8];
        let mut slots = [RxDatagramV4::new(&mut buffer)];
        for payload in [b"first".as_slice(), b"second"] {
            assert!(
                timeout(
                    Duration::from_millis(20),
                    receiver.recv_batch_from_v4(&mut slots)
                )
                .await
                .is_err()
            );
            sender.send_to(payload, address).unwrap();
            assert_eq!(
                timeout(
                    Duration::from_secs(2),
                    receiver.recv_batch_from_v4(&mut slots)
                )
                .await
                .expect("IPv4 receive did not recover after cancellation")
                .unwrap(),
                1
            );
            assert_eq!(&slots[0].buffer[..slots[0].len], payload);
            assert_eq!(
                SocketAddr::V4(slots[0].source),
                sender.local_addr().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn addressed_receive_propagates_invalid_source_errors() {
        let (sender, socket) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let mut receiver = AsyncBatchDgram::new(socket.into()).unwrap();
        sender.send(b"not-ipv4").unwrap();
        let mut buffer = [0; 16];
        let mut slots = [RxDatagramV4::new(&mut buffer)];
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                receiver.recv_batch_from_v4(&mut slots)
            )
            .await
            .expect("invalid source stalled the receiver")
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
