//! Batch IPv4 UDP I/O that preserves sources and leaves the socket unconnected.

use std::io;
use std::net::SocketAddrV4;
use std::os::fd::RawFd;

use super::BatchDgram;

/// A borrowed IPv4 UDP receive slot and the metadata for its last datagram.
pub struct RxDatagramV4<'a> {
    /// Destination buffer. Oversized datagrams can be truncated without a flag.
    pub buffer: &'a mut [u8],
    /// Copied payload length; valid only in the returned prefix of slots.
    pub len: usize,
    /// Source address; valid only in the returned prefix of slots.
    pub source: SocketAddrV4,
}

impl<'a> RxDatagramV4<'a> {
    /// Creates a receive slot with no valid datagram metadata yet.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            len: 0,
            source: SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0),
        }
    }
}

impl BatchDgram {
    /// Receives a prefix of `slots` from a non-blocking IPv4 UDP socket.
    ///
    /// Returns the datagram count, capped at [`Self::recv_capacity`]. Only that
    /// prefix has valid length and source metadata. Empty datagrams count as one.
    /// Each buffer must accommodate the largest datagram the caller accepts:
    /// Darwin silently truncates oversized datagrams without `MSG_TRUNC`.
    ///
    /// # Errors
    ///
    /// Returns syscall errors, including `WouldBlock`; retries `EINTR`.
    /// A non-IPv4 source returns `InvalidData` after consuming the batch.
    pub fn recv_batch_from_v4(
        &mut self,
        fd: RawFd,
        slots: &mut [RxDatagramV4<'_>],
    ) -> io::Result<usize> {
        let count = slots.len().min(Self::recv_capacity());
        if count == 0 {
            return Ok(0);
        }
        let mut addresses = [libc::sockaddr_in {
            #[cfg(target_os = "macos")]
            sin_len: 0,
            sin_family: 0,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        }; super::MAX_BATCH];
        for (i, slot) in slots.iter_mut().take(count).enumerate() {
            self.iovecs[i] = libc::iovec {
                iov_base: slot.buffer.as_mut_ptr().cast(),
                iov_len: slot.buffer.len(),
            };
        }
        self.setup_hdrs_recv(count);
        for (header, address) in self.hdrs[..count].iter_mut().zip(&mut addresses) {
            #[cfg(target_os = "linux")]
            let header = &mut header.msg_hdr;
            header.msg_name = std::ptr::from_mut(address).cast();
            header.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        }
        // All addresses and buffers remain live through the synchronous syscall.
        let received = self.platform_recv(fd, count)?;
        for (i, slot) in slots.iter_mut().take(received).enumerate() {
            #[cfg(target_os = "macos")]
            let (header, len) = (&self.hdrs[i], self.hdrs[i].msg_datalen);
            #[cfg(target_os = "linux")]
            let (header, len) = (&self.hdrs[i].msg_hdr, self.hdrs[i].msg_len as usize);
            let address = &addresses[i];
            if header.msg_namelen as usize != std::mem::size_of::<libc::sockaddr_in>()
                || address.sin_family != libc::AF_INET as libc::sa_family_t
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "batch source is not an IPv4 address",
                ));
            }
            slot.len = len;
            slot.source = SocketAddrV4::new(
                address.sin_addr.s_addr.to_ne_bytes().into(),
                u16::from_be(address.sin_port),
            );
        }
        Ok(received)
    }

    /// Sends a prefix of `bufs` to `destination` on an unconnected IPv4 UDP socket.
    ///
    /// The non-blocking socket remains unconnected and can receive from any peer.
    /// Returns the accepted datagram count, capped at [`Self::send_capacity`].
    /// A partial send accepts only `bufs[..n]`; the caller retains the suffix.
    ///
    /// # Errors
    ///
    /// Returns the syscall error when no datagram was accepted. `EINTR` is retried.
    pub fn send_batch_to_v4(
        &mut self,
        fd: RawFd,
        bufs: &[&[u8]],
        destination: SocketAddrV4,
    ) -> io::Result<usize> {
        let count = bufs.len().min(Self::send_capacity());
        if count == 0 {
            return Ok(0);
        }
        let mut address = libc::sockaddr_in {
            #[cfg(target_os = "macos")]
            sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: destination.port().to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(destination.ip().octets()),
            },
            sin_zero: [0; 8],
        };
        for (i, buf) in bufs.iter().take(count).enumerate() {
            self.iovecs[i] = libc::iovec {
                iov_base: buf.as_ptr().cast_mut().cast(),
                iov_len: buf.len(),
            };
        }
        self.setup_hdrs_send(count);
        for header in &mut self.hdrs[..count] {
            #[cfg(target_os = "linux")]
            let header = &mut header.msg_hdr;
            header.msg_name = (&raw mut address).cast();
            header.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        }
        // The address and payloads remain live until the synchronous syscall returns.
        self.platform_send(fd, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, UdpSocket};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    #[test]
    fn sends_distinct_datagrams_to_multiple_peers_without_connecting() {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").unwrap();
        let payloads = [b"first".as_slice(), b"", b"third-packet"];
        let mut batch = BatchDgram::new();
        let mut buf = [0; 64];
        for receiver in [&first, &second, &first] {
            receiver
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let SocketAddr::V4(address) = receiver.local_addr().unwrap() else {
                unreachable!()
            };
            assert_eq!(
                batch
                    .send_batch_to_v4(sender.as_raw_fd(), &[], address)
                    .unwrap(),
                0
            );
            let mut sent = 0;
            while sent < payloads.len() {
                let count = batch
                    .send_batch_to_v4(sender.as_raw_fd(), &payloads[sent..], address)
                    .unwrap();
                assert!(count > 0);
                sent += count;
            }
            for payload in payloads {
                let (len, peer) = receiver.recv_from(&mut buf).unwrap();
                assert_eq!(&buf[..len], payload);
                assert_eq!(peer, sender.local_addr().unwrap());
            }
        }
        assert!(sender.peer_addr().is_err());
    }

    #[test]
    fn partial_send_reports_only_the_accepted_prefix() {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let SocketAddr::V4(address) = receiver.local_addr().unwrap() else {
            unreachable!()
        };
        let oversized = vec![0; 65536];
        let mut batch = BatchDgram::new();
        let payloads = [b"accepted".as_slice(), oversized.as_slice(), b"unsent"];
        assert_eq!(
            batch
                .send_batch_to_v4(sender.as_raw_fd(), &payloads, address)
                .unwrap(),
            1
        );
        let error = batch
            .send_batch_to_v4(sender.as_raw_fd(), &payloads[1..], address)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EMSGSIZE));
        let mut buf = [0; 64];
        assert_eq!(receiver.recv(&mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"accepted");
        receiver.set_nonblocking(true).unwrap();
        assert_eq!(
            receiver.recv(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    fn wait_readable(fd: RawFd) {
        let mut event = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event is one initialized pollfd and remains live during poll.
        assert_eq!(unsafe { libc::poll(&raw mut event, 1, 2000) }, 1);
        assert_ne!(event.revents & libc::POLLIN, 0);
    }

    #[test]
    fn receives_sources_empty_datagrams_and_reuses_headers() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_nonblocking(true).unwrap();
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let payloads: [&[u8]; 3] = [b"first", b"", b"last"];
        let mut io = BatchDgram::new();
        let mut buffers = [[0; 64]; 3];
        for iteration in 0..3 {
            let peers = if iteration % 2 == 0 {
                [&first, &second, &first]
            } else {
                [&second, &first, &second]
            };
            for (peer, payload) in peers.iter().zip(payloads) {
                assert_eq!(
                    peer.send_to(payload, receiver.local_addr().unwrap())
                        .unwrap(),
                    payload.len()
                );
            }
            let mut offset = 0;
            while offset < payloads.len() {
                let mut chunks = buffers.iter_mut();
                let mut slots: [_; 3] =
                    std::array::from_fn(|_| RxDatagramV4::new(chunks.next().unwrap()));
                wait_readable(receiver.as_raw_fd());
                let n = io
                    .recv_batch_from_v4(receiver.as_raw_fd(), &mut slots[..payloads.len() - offset])
                    .unwrap();
                assert!(n > 0);
                for (i, slot) in slots[..n].iter().enumerate() {
                    assert_eq!(&slot.buffer[..slot.len], payloads[offset + i]);
                    assert_eq!(
                        SocketAddr::V4(slot.source),
                        peers[offset + i].local_addr().unwrap()
                    );
                }
                offset += n;
            }
        }
        assert_eq!(
            io.recv_batch_from_v4(receiver.as_raw_fd(), &mut [])
                .unwrap(),
            0
        );
        let mut storage = [0; 64];
        let mut slots = [RxDatagramV4::new(&mut storage)];
        assert_eq!(
            io.recv_batch_from_v4(receiver.as_raw_fd(), &mut slots)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let SocketAddr::V4(peer) = first.local_addr().unwrap() else {
            unreachable!()
        };
        assert_eq!(
            io.send_batch_to_v4(receiver.as_raw_fd(), &[b"reply"], peer)
                .unwrap(),
            1
        );
        assert_eq!(first.recv(&mut storage).unwrap(), 5);
        first
            .send_to(b"raw", receiver.local_addr().unwrap())
            .unwrap();
        wait_readable(receiver.as_raw_fd());
        let entries = io
            .recv_batch(receiver.as_raw_fd(), &mut [&mut storage])
            .unwrap();
        assert_eq!(entries[0].len, 3);
        assert_eq!(&storage[..3], b"raw");
    }

    #[test]
    fn addressed_receive_respects_slot_boundaries_and_rejects_unix_sources() {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_nonblocking(true).unwrap();
        let mut io = BatchDgram::new();
        for length in [16, 17, 1024] {
            sender
                .send_to(&vec![0xBB; length], receiver.local_addr().unwrap())
                .unwrap();
            wait_readable(receiver.as_raw_fd());
            let mut guarded = [0xA5; 48];
            let mut slots = [RxDatagramV4::new(&mut guarded[..17])];
            assert_eq!(
                io.recv_batch_from_v4(receiver.as_raw_fd(), &mut slots)
                    .unwrap(),
                1
            );
            assert_eq!(slots[0].len, length.min(17));
            assert_eq!(
                &slots[0].buffer[..slots[0].len],
                &vec![0xBB; length.min(17)]
            );
            assert_eq!(&guarded[17..], &[0xA5; 31]);
        }
        let (first, second) = std::os::unix::net::UnixDatagram::pair().unwrap();
        second.set_nonblocking(true).unwrap();
        first.send(b"not-ipv4").unwrap();
        let mut buffer = [0; 32];
        assert_eq!(
            io.recv_batch_from_v4(second.as_raw_fd(), &mut [RxDatagramV4::new(&mut buffer)])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        first.send(b"still-raw").unwrap();
        assert_eq!(
            io.recv_batch(second.as_raw_fd(), &mut [&mut buffer])
                .unwrap()[0]
                .len,
            9
        );
    }
}
