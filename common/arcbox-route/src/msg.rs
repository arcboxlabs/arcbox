//! Routing socket message construction and I/O.
//!
//! Builds `rt_msghdr` + variable-length sockaddr payloads for PF_ROUTE
//! operations (RTM_ADD, RTM_DELETE, RTM_CHANGE, RTM_GET).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// Sequence counter for correlating routing socket request/reply pairs.
static SEQ: AtomicI32 = AtomicI32::new(1);

/// BSD routing socket sockaddr alignment.
///
/// Rounds `sa_len` up to a 4-byte boundary with a minimum of 4 bytes.
/// Mirrors XNU's internal `SA_SIZE` / `ROUNDUP` macro.
pub const fn sa_rlen(sa_len: usize) -> usize {
    if sa_len == 0 { 4 } else { (sa_len + 3) & !3 }
}

/// Maximum routing message buffer size (header + up to 8 sockaddrs).
const MAX_MSG_SIZE: usize = 512;

/// Routing message type for [`build_msg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Add,
    Delete,
    Change,
    Get,
}

impl MsgType {
    const fn as_rtm(self) -> i32 {
        match self {
            Self::Add => libc::RTM_ADD,
            Self::Delete => libc::RTM_DELETE,
            Self::Change => libc::RTM_CHANGE,
            Self::Get => libc::RTM_GET,
        }
    }
}

/// Builds a routing socket message with the given sockaddrs.
///
/// # Layout
///
/// ```text
/// [rt_msghdr][DST sockaddr_in][GATEWAY sockaddr_dl (optional)][NETMASK sockaddr_in]
/// ```
///
/// `gateway_dl` is omitted for `Delete` and `Get` without interface context.
/// Each sockaddr is padded to 4-byte alignment per BSD convention.
pub fn build_msg(
    msg_type: MsgType,
    dst: &libc::sockaddr_in,
    gateway: Option<&libc::sockaddr_dl>,
    netmask: &libc::sockaddr_in,
) -> io::Result<Vec<u8>> {
    let hdr_len = std::mem::size_of::<libc::rt_msghdr>();
    let dst_len = sa_rlen(dst.sin_len as usize);
    let gw_len = gateway.map_or(0, |g| sa_rlen(g.sdl_len as usize));
    let mask_len = sa_rlen(netmask.sin_len as usize);
    let total = hdr_len + dst_len + gw_len + mask_len;

    if total > MAX_MSG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "routing message exceeds maximum size",
        ));
    }

    let mut buf = vec![0u8; total];

    // Fill rt_msghdr.
    // Safety: buf is Vec-allocated (heap, properly aligned) and large enough
    // for rt_msghdr. Vec guarantees sufficient alignment for any primitive.
    #[allow(clippy::cast_ptr_alignment)]
    let hdr = unsafe { &mut *buf.as_mut_ptr().cast::<libc::rt_msghdr>() };
    hdr.rtm_msglen = total as u16;
    hdr.rtm_version = libc::RTM_VERSION as u8;
    hdr.rtm_type = msg_type.as_rtm() as u8;
    hdr.rtm_seq = SEQ.fetch_add(1, Ordering::Relaxed);
    // Safety: getpid is always safe.
    hdr.rtm_pid = unsafe { libc::getpid() };

    let mut addrs = libc::RTA_DST | libc::RTA_NETMASK;
    // Interface-based route: RTF_UP | RTF_STATIC, no RTF_GATEWAY.
    let flags = libc::RTF_UP | libc::RTF_STATIC;

    if gateway.is_some() {
        addrs |= libc::RTA_GATEWAY;
    }
    hdr.rtm_addrs = addrs;
    hdr.rtm_flags = flags;

    // Append sockaddrs in RTA bit order: DST, GATEWAY, NETMASK.
    let mut offset = hdr_len;

    // DST (sockaddr_in).
    // Safety: dst is a valid sockaddr_in; dst_len fits in buf.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(dst).cast::<u8>(),
            buf.as_mut_ptr().add(offset),
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }
    offset += dst_len;

    // GATEWAY (sockaddr_dl, optional).
    if let Some(gw) = gateway {
        // Safety: gw is a valid sockaddr_dl; gw_len fits in buf.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(gw).cast::<u8>(),
                buf.as_mut_ptr().add(offset),
                std::mem::size_of::<libc::sockaddr_dl>(),
            );
        }
        offset += gw_len;
    }

    // NETMASK (sockaddr_in).
    // Safety: netmask is a valid sockaddr_in; mask_len fits in buf.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(netmask).cast::<u8>(),
            buf.as_mut_ptr().add(offset),
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }

    Ok(buf)
}

/// Reply from a routing socket write operation.
#[derive(Debug)]
pub struct RouteReply {
    /// The `rtm_errno` field from the kernel's reply.
    pub errno: i32,
    /// The interface index from the reply (`rtm_index`).
    pub ifindex: u16,
    /// The flags from the reply (`rtm_flags`).
    pub flags: i32,
}

/// Opens a PF_ROUTE socket, writes `msg`, and reads back the kernel reply.
///
/// Each call opens a fresh socket so that concurrent operations don't
/// interleave replies.
pub fn route_send(msg: &[u8]) -> io::Result<RouteReply> {
    // Safety: socket(PF_ROUTE, SOCK_RAW, AF_UNSPEC) is always safe.
    let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: fd is valid from socket() above.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    // Disable loopback of our own messages to reduce noise.
    let off: libc::c_int = 0;
    // Safety: setsockopt with valid fd and properly sized value.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_USELOOPBACK,
            (&raw const off).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    // Safety: write with valid fd and buffer from build_msg.
    let n = unsafe { libc::write(fd.as_raw_fd(), msg.as_ptr().cast(), msg.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    // Read the kernel's reply. The kernel echoes back the rt_msghdr with
    // rtm_errno filled in.
    let mut reply = [0u8; MAX_MSG_SIZE];
    let n = unsafe { libc::read(fd.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len()) };
    let min_reply_size: isize = std::mem::size_of::<libc::rt_msghdr>()
        .try_into()
        .expect("rt_msghdr size fits in isize");
    if n < min_reply_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("routing socket reply too short: {n} bytes"),
        ));
    }

    // Safety: n >= sizeof(rt_msghdr), and reply is stack-allocated with
    // natural alignment sufficient for rt_msghdr (u8 array on stack).
    #[allow(clippy::cast_ptr_alignment)]
    let hdr = unsafe { &*reply.as_ptr().cast::<libc::rt_msghdr>() };
    Ok(RouteReply {
        errno: hdr.rtm_errno,
        ifindex: hdr.rtm_index,
        flags: hdr.rtm_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ipv4Net;
    use crate::sockaddr;

    #[test]
    fn sa_rlen_alignment() {
        assert_eq!(sa_rlen(0), 4);
        assert_eq!(sa_rlen(1), 4);
        assert_eq!(sa_rlen(4), 4);
        assert_eq!(sa_rlen(5), 8);
        assert_eq!(sa_rlen(16), 16);
        assert_eq!(sa_rlen(20), 20);
        assert_eq!(sa_rlen(21), 24);
    }

    #[test]
    fn build_msg_add_with_gateway() {
        let net: Ipv4Net = "172.16.0.0/12".parse().unwrap();
        let dst = sockaddr::make_dst(net);
        let gw = sockaddr::make_gateway_dl_with_index("bridge100", 42);
        let mask = sockaddr::make_netmask(net);

        let buf = build_msg(MsgType::Add, &dst, Some(&gw), &mask).unwrap();

        // Check rt_msghdr fields.
        let hdr = unsafe { &*buf.as_ptr().cast::<libc::rt_msghdr>() };
        assert_eq!(hdr.rtm_version, libc::RTM_VERSION as u8);
        assert_eq!(hdr.rtm_type, libc::RTM_ADD as u8);
        assert_eq!(
            hdr.rtm_addrs,
            libc::RTA_DST | libc::RTA_GATEWAY | libc::RTA_NETMASK
        );
        assert_ne!(hdr.rtm_flags & libc::RTF_UP, 0);
        assert_ne!(hdr.rtm_flags & libc::RTF_STATIC, 0);
        // Interface route: RTF_GATEWAY must NOT be set.
        assert_eq!(hdr.rtm_flags & libc::RTF_GATEWAY, 0);
        assert_eq!(hdr.rtm_msglen as usize, buf.len());
    }

    #[test]
    fn build_msg_delete_no_gateway() {
        let net: Ipv4Net = "10.0.0.0/8".parse().unwrap();
        let dst = sockaddr::make_dst(net);
        let mask = sockaddr::make_netmask(net);

        let buf = build_msg(MsgType::Delete, &dst, None, &mask).unwrap();

        let hdr = unsafe { &*buf.as_ptr().cast::<libc::rt_msghdr>() };
        assert_eq!(hdr.rtm_type, libc::RTM_DELETE as u8);
        assert_eq!(hdr.rtm_addrs, libc::RTA_DST | libc::RTA_NETMASK);

        // Verify total length: hdr + DST(16) + NETMASK(16), no gateway.
        let expected = std::mem::size_of::<libc::rt_msghdr>()
            + sa_rlen(std::mem::size_of::<libc::sockaddr_in>())
            + sa_rlen(std::mem::size_of::<libc::sockaddr_in>());
        assert_eq!(buf.len(), expected);
    }

    #[test]
    fn build_msg_change_type() {
        let net: Ipv4Net = "10.0.0.0/8".parse().unwrap();
        let dst = sockaddr::make_dst(net);
        let gw = sockaddr::make_gateway_dl_with_index("bridge100", 1);
        let mask = sockaddr::make_netmask(net);

        let buf = build_msg(MsgType::Change, &dst, Some(&gw), &mask).unwrap();

        let hdr = unsafe { &*buf.as_ptr().cast::<libc::rt_msghdr>() };
        assert_eq!(hdr.rtm_type, libc::RTM_CHANGE as u8);
    }

    #[test]
    fn build_msg_sockaddr_order() {
        let net: Ipv4Net = "192.168.0.0/16".parse().unwrap();
        let dst = sockaddr::make_dst(net);
        let gw = sockaddr::make_gateway_dl_with_index("bridge100", 5);
        let mask = sockaddr::make_netmask(net);

        let buf = build_msg(MsgType::Add, &dst, Some(&gw), &mask).unwrap();
        let hdr_len = std::mem::size_of::<libc::rt_msghdr>();

        // First sockaddr: DST → AF_INET.
        assert_eq!(buf[hdr_len + 1], libc::AF_INET as u8); // sin_family at offset 1

        // Second sockaddr: GATEWAY → AF_LINK.
        let gw_offset = hdr_len + sa_rlen(std::mem::size_of::<libc::sockaddr_in>());
        assert_eq!(buf[gw_offset + 1], libc::AF_LINK as u8); // sdl_family at offset 1

        // Third sockaddr: NETMASK → AF_INET.
        let mask_offset = gw_offset + sa_rlen(std::mem::size_of::<libc::sockaddr_dl>());
        assert_eq!(buf[mask_offset + 1], libc::AF_INET as u8);
    }
}
