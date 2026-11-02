//! Drives a back-end the way QEMU does, over a socketpair, and checks what
//! the front-end receives.
#![cfg(target_os = "macos")]

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use arcbox_vhost_user::{Backend, VhostUserDevice, Vrings};

const F_VERSION_1: u64 = 1 << 32;
const P_REPLY_ACK: u64 = 1 << 3;

struct Frontend {
    sock: UnixStream,
}

impl Frontend {
    fn send(&self, request: u32, flags: u32, payload: &[u8], fds: &[RawFd]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&request.to_ne_bytes());
        bytes.extend_from_slice(&(1 | flags).to_ne_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(payload);
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0u64; 16];
        // SAFETY: zeroed msghdr is valid; fields set below.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            // SAFETY: the control buffer fits one cmsg with these descriptors.
            unsafe {
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(size_of_val(fds) as u32) as _;
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
        let n = unsafe { libc::sendmsg(self.sock.as_raw_fd(), &raw const msg, 0) };
        assert_eq!(n as usize, bytes.len());
    }

    fn reply(&self, request: u32) -> Vec<u8> {
        use std::io::Read;
        let mut header = [0u8; 12];
        (&self.sock).read_exact(&mut header).unwrap();
        assert_eq!(u32::from_ne_bytes(header[..4].try_into().unwrap()), request);
        assert_eq!(
            u32::from_ne_bytes(header[4..8].try_into().unwrap()),
            1 | (1 << 2)
        );
        let mut payload = vec![0u8; u32::from_ne_bytes(header[8..].try_into().unwrap()) as usize];
        (&self.sock).read_exact(&mut payload).unwrap();
        payload
    }

    fn get_u64(&self, request: u32) -> u64 {
        self.send(request, 0, &[], &[]);
        u64::from_ne_bytes(self.reply(request).try_into().unwrap())
    }
}

/// A device with an 8-byte config space, like vhost-user-vsock's guest CID.
struct WithConfig;

impl VhostUserDevice for WithConfig {
    fn num_queues(&self) -> usize {
        2
    }

    fn features(&self) -> u64 {
        F_VERSION_1
    }

    fn protocol_features(&self) -> arcbox_vhost_user::ProtocolFeatures {
        arcbox_vhost_user::ProtocolFeatures::CONFIG
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) -> arcbox_vhost_user::Result<()> {
        let config = 0x1122_3344_5566_7788u64.to_le_bytes();
        data.copy_from_slice(&config[offset as usize..offset as usize + data.len()]);
        Ok(())
    }

    fn process_queue(&mut self, _: u16, _: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        Ok(())
    }
}

fn get_config(fe: &Frontend, offset: u32, size: u32) -> Vec<u8> {
    let mut p = offset.to_ne_bytes().to_vec();
    p.extend_from_slice(&size.to_ne_bytes());
    p.extend_from_slice(&0u32.to_ne_bytes());
    p.extend(std::iter::repeat_n(0u8, size as usize));
    fe.send(24, 0, &p, &[]);
    fe.reply(24)
}

#[test]
fn config_space_is_served_only_once_negotiated() {
    let (front, back) = UnixStream::pair().unwrap();
    front
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let handle = Backend::new("test", WithConfig).spawn(back).unwrap();
    let fe = Frontend { sock: front };
    const P_CONFIG: u64 = 1 << 9;
    assert_eq!(fe.get_u64(15) & P_CONFIG, P_CONFIG);
    fe.send(16, 0, &(P_CONFIG | P_REPLY_ACK).to_ne_bytes(), &[]);

    // Upper half of the 8-byte config: the reply echoes offset and size and
    // carries exactly the bytes asked for.
    let reply = get_config(&fe, 4, 4);
    assert_eq!(&reply[..4], &4u32.to_ne_bytes());
    assert_eq!(&reply[4..8], &4u32.to_ne_bytes());
    assert_eq!(&reply[12..], &[0x44, 0x33, 0x22, 0x11]);
    drop(fe);
    handle.join().unwrap();

    // Without CONFIG negotiated the request ends the connection.
    let (front, back) = UnixStream::pair().unwrap();
    let handle = Backend::new("test", WithConfig).spawn(back).unwrap();
    let fe = Frontend { sock: front };
    let mut p = 0u32.to_ne_bytes().to_vec();
    p.extend_from_slice(&8u32.to_ne_bytes());
    p.extend_from_slice(&[0u8; 12]);
    fe.send(24, 0, &p, &[]);
    assert!(handle.join().is_err());
}
