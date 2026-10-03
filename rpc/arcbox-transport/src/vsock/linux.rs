use super::addr::VsockAddr;
use super::stream::VsockStream;
use crate::error::{Result, TransportError};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

fn sockaddr_vm(cid: u32, port: u32) -> libc::sockaddr_vm {
    libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    }
}

/// Creates a vsock socket.
pub fn create_socket() -> Result<OwnedFd> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|e| TransportError::io(e.into()))?;
    Ok(fd)
}

/// Connects to a vsock address and returns a ready [`VsockStream`].
pub fn connect_vsock(addr: VsockAddr) -> Result<VsockStream> {
    let fd = create_socket()?;

    let sockaddr = sockaddr_vm(addr.cid, addr.port);

    // SAFETY: sockaddr is a fully initialised sockaddr_vm.
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const sockaddr).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };

    if result < 0 {
        let err = io::Error::last_os_error();
        return Err(TransportError::ConnectionRefused(err.to_string()));
    }

    VsockStream::from_fd(fd).map_err(TransportError::io)
}

/// Binds to a vsock port and returns the listening socket fd.
pub fn bind_vsock(port: u32) -> Result<OwnedFd> {
    let fd = create_socket()?;

    let sockaddr = sockaddr_vm(VsockAddr::CID_ANY, port);

    // SAFETY: sockaddr is a fully initialised sockaddr_vm.
    let result = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const sockaddr).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(TransportError::io(io::Error::last_os_error()));
    }

    // SAFETY: fd is a valid bound socket.
    let result = unsafe { libc::listen(fd.as_raw_fd(), 128) };
    if result < 0 {
        return Err(TransportError::io(io::Error::last_os_error()));
    }

    Ok(fd)
}

/// Accepts a connection on a vsock listener.
pub fn accept_vsock(listener_fd: &OwnedFd) -> Result<(VsockStream, VsockAddr)> {
    let mut sockaddr = sockaddr_vm(0, 0);
    let mut len = mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

    // SAFETY: listener_fd is a valid socket; sockaddr is correctly sized.
    // Use accept4 with SOCK_CLOEXEC to prevent fd leaks into child processes.
    let fd = unsafe {
        libc::accept4(
            listener_fd.as_raw_fd(),
            (&raw mut sockaddr).cast::<libc::sockaddr>(),
            &raw mut len,
            libc::SOCK_CLOEXEC,
        )
    };

    if fd < 0 {
        return Err(TransportError::io(io::Error::last_os_error()));
    }

    // SAFETY: fd was just returned from accept4() and is valid.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = VsockAddr::new(sockaddr.svm_cid, sockaddr.svm_port);

    Ok((
        VsockStream::from_fd(owned_fd).map_err(TransportError::io)?,
        addr,
    ))
}
