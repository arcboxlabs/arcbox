//! Batch sends to an explicit IPv4 peer without connecting the socket.

use std::io;
use std::net::SocketAddrV4;
use std::os::fd::RawFd;

use super::BatchDgram;

impl BatchDgram {
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
}
