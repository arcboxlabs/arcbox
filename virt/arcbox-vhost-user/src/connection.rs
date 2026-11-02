//! Framed vhost-user messages over a Unix stream socket, with the file
//! descriptors that ride along as `SCM_RIGHTS`.
//!
//! macOS has no `MSG_CMSG_CLOEXEC`, so every received descriptor is marked
//! close-on-exec right after `recvmsg` — the daemon spawns QEMU and must not
//! leak guest-memory or notifier descriptors into it or anything else.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::error::{Error, Result};
use crate::protocol::{HEADER_SIZE, Header, MAX_FDS, encode_reply};

/// One request from the front-end.
#[derive(Debug)]
pub struct Message {
    /// The decoded header.
    pub header: Header,
    /// `header.size` payload bytes.
    pub payload: Vec<u8>,
    /// Descriptors received with the message, in sender order.
    pub fds: Vec<OwnedFd>,
}

/// The back-end end of a vhost-user socket.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Wraps a connected stream; it must be in blocking mode.
    #[must_use]
    pub const fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// The underlying socket.
    #[must_use]
    pub const fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Receives the next request; `None` when the front-end closed the socket
    /// at a message boundary.
    pub fn recv(&self) -> Result<Option<Message>> {
        let mut fds = Vec::new();
        let mut header = [0u8; HEADER_SIZE];
        if !self.recv_exact(&mut header, &mut fds, true)? {
            return Ok(None);
        }
        let header = Header::decode(&header)?;
        let mut payload = vec![0u8; header.size as usize];
        self.recv_exact(&mut payload, &mut fds, false)?;
        Ok(Some(Message {
            header,
            payload,
            fds,
        }))
    }

    /// Sends a reply to `request`.
    pub fn send_reply(&self, request: u32, payload: &[u8]) -> Result<()> {
        (&self.stream).write_all(&encode_reply(request, payload))?;
        Ok(())
    }

    /// Fills `buf`, collecting any descriptors that arrive on the way.
    /// Returns `false` on EOF before the first byte when `eof_ok`.
    fn recv_exact(&self, buf: &mut [u8], fds: &mut Vec<OwnedFd>, eof_ok: bool) -> Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.recv_some(&mut buf[filled..], fds)?;
            if n == 0 {
                if filled == 0 && eof_ok {
                    return Ok(false);
                }
                return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            filled += n;
        }
        Ok(true)
    }

    /// One `recvmsg`, retried on `EINTR`.
    fn recv_some(&self, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> Result<usize> {
        // Room for MAX_FDS descriptors; u64 storage keeps cmsghdr aligned.
        // SAFETY: CMSG_SPACE is pure arithmetic on its argument.
        let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) } as usize;
        let mut control = vec![0u64; space.div_ceil(size_of::<u64>())];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        loop {
            // SAFETY: an all-zero msghdr is a valid value; the fields that
            // matter are set right below.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as libc::socklen_t;
            // SAFETY: `msg` points at `iov` (valid for `buf.len()` bytes) and
            // `control` (valid for `space` bytes); both outlive the call.
            let n = unsafe { libc::recvmsg(self.stream.as_raw_fd(), &raw mut msg, 0) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            // Take ownership of every received descriptor before anything can
            // fail, so an error path still closes them.
            // SAFETY: `msg` was filled by a successful recvmsg; the control
            // buffer is valid and the kernel set msg_controllen.
            unsafe { collect_fds(&msg, fds) }?;
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                return Err(Error::Protocol(format!(
                    "front-end sent more than {MAX_FDS} descriptors in one message"
                )));
            }
            return Ok(n as usize);
        }
    }
}

/// Moves every `SCM_RIGHTS` descriptor in `msg` into `fds`, marking each
/// close-on-exec.
///
/// # Safety
/// `msg` must come from a successful `recvmsg` whose control buffer is still
/// alive.
unsafe fn collect_fds(msg: &libc::msghdr, fds: &mut Vec<OwnedFd>) -> Result<()> {
    // SAFETY: guaranteed by the caller.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        // SAFETY: CMSG_FIRSTHDR/CMSG_NXTHDR return headers inside the control
        // buffer the kernel filled.
        let (level, kind, len) =
            unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
        if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
            // SAFETY: as above; CMSG_LEN(0) is the header size.
            let header = unsafe { libc::CMSG_LEN(0) } as usize;
            let count = (len as usize).saturating_sub(header) / size_of::<RawFd>();
            // SAFETY: CMSG_DATA points at `count` descriptors written by the
            // kernel, not necessarily aligned for RawFd.
            let data = unsafe { libc::CMSG_DATA(cmsg) };
            for i in 0..count {
                let mut bytes = [0u8; size_of::<RawFd>()];
                // SAFETY: `i < count`, so the bytes lie within the received data.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.add(i * bytes.len()),
                        bytes.as_mut_ptr(),
                        bytes.len(),
                    );
                }
                let raw = RawFd::from_ne_bytes(bytes);
                // SAFETY: the kernel just installed `raw` in our table and no
                // one else owns it.
                let fd = unsafe { OwnedFd::from_raw_fd(raw) };
                set_cloexec(&fd)?;
                fds.push(fd);
            }
        }
        // SAFETY: walks to the next header within the same buffer.
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    Ok(())
}

fn set_cloexec(fd: &OwnedFd) -> Result<()> {
    // SAFETY: F_SETFD on a descriptor we own only changes its flags.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::Request;

    /// Sends `header + payload` with `fds` attached in one `sendmsg`, the way
    /// QEMU does.
    pub fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: arithmetic only.
        let space = unsafe { libc::CMSG_SPACE(size_of_val(fds) as u32) } as usize;
        let mut control = vec![0u64; space.div_ceil(8)];
        // SAFETY: zeroed msghdr is valid; fields set below.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as libc::socklen_t;
            // SAFETY: the control buffer is large enough for one cmsg with
            // `fds.len()` descriptors.
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(cmsg),
                    size_of_val(fds),
                );
            }
        }
        // SAFETY: msg references live buffers.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &raw const msg, 0) };
        assert_eq!(
            n as usize,
            bytes.len(),
            "{}",
            std::io::Error::last_os_error()
        );
    }

    /// Encodes a front-end request.
    pub fn request(request: Request, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(request as u32).to_ne_bytes());
        out.extend_from_slice(&(0x1 | flags).to_ne_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn descriptors_arrive_with_their_message_and_are_cloexec() {
        let (front, back) = UnixStream::pair().unwrap();
        let conn = Connection::new(back);
        let file = tempfile::tempfile().unwrap();
        let payload = 7u64.to_ne_bytes();
        send_with_fds(
            &front,
            &request(Request::SetVringKick, 0, &payload),
            &[file.as_raw_fd(), file.as_raw_fd()],
        );
        // A second message without descriptors right behind it must not
        // inherit any.
        send_with_fds(&front, &request(Request::GetFeatures, 0, &[]), &[]);

        let first = conn.recv().unwrap().unwrap();
        assert_eq!(first.header.request, Request::SetVringKick as u32);
        assert_eq!(first.payload, payload);
        assert_eq!(first.fds.len(), 2);
        for fd in &first.fds {
            // SAFETY: F_GETFD on an owned descriptor.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
        let second = conn.recv().unwrap().unwrap();
        assert_eq!(second.header.request, Request::GetFeatures as u32);
        assert!(second.fds.is_empty());

        drop(front);
        assert!(conn.recv().unwrap().is_none());
    }

    #[test]
    fn eof_inside_a_message_is_an_error() {
        let (front, back) = UnixStream::pair().unwrap();
        let conn = Connection::new(back);
        let bytes = request(Request::SetFeatures, 0, &1u64.to_ne_bytes());
        send_with_fds(&front, &bytes[..HEADER_SIZE + 3], &[]);
        drop(front);
        assert!(conn.recv().is_err());
    }
}
