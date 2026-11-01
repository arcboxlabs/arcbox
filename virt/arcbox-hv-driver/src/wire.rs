//! Framing on the control socket.
//!
//! A frame is a big-endian `u32` length, then that many bytes of JSON. It
//! may carry one file descriptor, sent as `SCM_RIGHTS` with its first
//! byte, so the receiver collects it while it reads the length.
//!
//! Both ends use blocking sockets: the driver from `spawn_blocking` or a
//! thread of its own, the VMM process from one thread per connection.

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest frame either end accepts; a debug snapshot is the biggest.
const MAX_FRAME: usize = 16 << 20;

/// Writes `msg` as one frame, with `fd` attached when given.
pub fn send<T: Serialize>(
    sock: &UnixStream,
    msg: &T,
    fd: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);

    let fds: Vec<RawFd> = fd.iter().map(AsRawFd::as_raw_fd).collect();
    let mut sent = 0;
    while sent < frame.len() {
        let rights = [ControlMessage::ScmRights(&fds)];
        let cmsgs: &[ControlMessage<'_>] = if sent == 0 && !fds.is_empty() {
            &rights
        } else {
            &[]
        };
        match sendmsg::<()>(
            sock.as_raw_fd(),
            &[IoSlice::new(&frame[sent..])],
            cmsgs,
            MsgFlags::empty(),
            None,
        ) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => sent += n,
            Err(nix::errno::Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Reads one frame, and the descriptor that came with it. A closed peer
/// before the first byte is `UnexpectedEof`.
pub fn recv<T: DeserializeOwned>(sock: &UnixStream) -> io::Result<(T, Option<OwnedFd>)> {
    let mut fd = None;
    let mut len = [0u8; 4];
    read_exact(sock, &mut len, &mut fd)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    read_exact(sock, &mut body, &mut fd)?;
    let msg =
        serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((msg, fd))
}

/// Fills `buf`, keeping the first descriptor that arrives and closing any
/// other.
fn read_exact(sock: &UnixStream, buf: &mut [u8], fd: &mut Option<OwnedFd>) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        let mut space = nix::cmsg_space!([RawFd; 1]);
        let mut iov = [IoSliceMut::new(&mut buf[got..])];
        let msg = match recvmsg::<()>(
            sock.as_raw_fd(),
            &mut iov,
            Some(&mut space),
            MsgFlags::empty(),
        ) {
            Ok(msg) => msg,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        };
        for cmsg in msg.cmsgs().map_err(io::Error::from)? {
            if let ControlMessageOwned::ScmRights(raws) = cmsg {
                for raw in raws {
                    // SAFETY: the kernel just installed `raw` in this
                    // process for us; nothing else owns it.
                    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                    // macOS has no MSG_CMSG_CLOEXEC: keep the descriptor
                    // out of VMM processes spawned from here on.
                    // SAFETY: fcntl on a descriptor we own.
                    unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
                    if fd.is_none() {
                        *fd = Some(owned);
                    }
                }
            }
        }
        if msg.bytes == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += msg.bytes;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn a_frame_carries_its_message_and_descriptor() {
        let (a, b) = UnixStream::pair().unwrap();
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        send(&a, &("hello", 7u32), Some(ours.as_fd())).unwrap();
        send(&a, &"second", None).unwrap();

        let ((text, n), fd) = recv::<(String, u32)>(&b).unwrap();
        assert_eq!((text.as_str(), n), ("hello", 7));
        // The received descriptor is the same socket: what goes in one end
        // comes out of the peer we kept.
        let mut passed = UnixStream::from(fd.expect("a descriptor"));
        passed.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        theirs.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        let (second, fd) = recv::<String>(&b).unwrap();
        assert_eq!(second, "second");
        assert!(fd.is_none(), "a descriptor leaked into the next frame");
    }

    #[test]
    fn a_closed_peer_is_eof_and_an_oversized_length_is_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        let err = recv::<String>(&b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&u32::MAX.to_be_bytes()).unwrap();
        let err = recv::<String>(&b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
