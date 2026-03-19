//! AF_PACKET socket for raw L2 frame I/O on a named interface.
//!
//! Each `read()` on the returned fd yields one complete Ethernet frame with
//! message-boundary semantics — identical to a `SOCK_DGRAM` socketpair.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

/// Opens a non-blocking `AF_PACKET SOCK_RAW` socket bound to `iface_name`.
///
/// The socket captures all L2 Ethernet frames arriving on the interface.
/// Writing to the socket injects frames onto the interface.
///
/// # Errors
///
/// Returns an error if the socket cannot be created, the interface does not
/// exist, or binding fails.
pub fn open_af_packet(iface_name: &str) -> io::Result<OwnedFd> {
    // SAFETY: socket(2) with valid AF_PACKET constants.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW,
            (libc::ETH_P_ALL as u16).to_be() as libc::c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a valid file descriptor returned by socket().
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Resolve interface index.
    let ifindex = if_nametoindex(iface_name)?;

    // Bind to the specific interface.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    sll.sll_ifindex = ifindex;

    // SAFETY: owned fd is valid; sll is properly initialized.
    let ret = unsafe {
        libc::bind(
            fd,
            (&raw const sll).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    // Set non-blocking.
    // SAFETY: fd is valid.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is valid, flags are valid.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(owned)
}

/// Resolves an interface name to its index via `if_nametoindex(3)`.
fn if_nametoindex(name: &str) -> io::Result<libc::c_int> {
    let c_name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL"))?;
    // SAFETY: c_name is a valid null-terminated C string.
    let idx = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if idx == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface not found: {name}"),
        ));
    }
    Ok(idx as libc::c_int)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_if_nametoindex_loopback() {
        // lo should always exist on Linux.
        let idx = if_nametoindex("lo").unwrap();
        assert!(idx > 0);
    }

    #[test]
    fn test_if_nametoindex_nonexistent() {
        let err = if_nametoindex("nonexistent_iface_42").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_open_af_packet_loopback() {
        // AF_PACKET requires CAP_NET_RAW; skip if not root.
        if !is_root() {
            eprintln!("SKIP test_open_af_packet_loopback — requires root");
            return;
        }
        let fd = open_af_packet("lo").unwrap();
        // Verify the fd is valid and non-blocking.
        let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&fd), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }
}
