//! The socketpair behind a host-initiated connection.
//!
//! Every backend that dials guest ports from the host (HV, vhost-user) hands
//! the daemon one end of a Unix stream socketpair and gives the other to
//! [`super::VsockConnectionManager::allocate`]; the device relays between
//! that end and the guest's queues.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Send and receive buffer of both ends. macOS defaults to ~8 KiB, which
/// truncated large RPC responses on the device's write path (ABX-365);
/// 1 MiB fits anything the agent emits.
const SOCKET_BUFFER_BYTES: libc::c_int = 1 << 20;

/// Creates a connection's socketpair and returns `(daemon end, device end)`.
///
/// Both ends are close-on-exec with 1 MiB buffers. The device end is
/// non-blocking: the device peeks and reads it from its event loop. The
/// daemon end stays blocking; tokio's `AsyncFd` sets `O_NONBLOCK` when it
/// wraps the fd.
pub fn host_socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` is a valid 2-element array; socketpair writes two fds
    // into it on success.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds are fresh from socketpair with sole ownership;
    // `OwnedFd`'s drop closes them on the error paths below.
    let (daemon, device) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    // SAFETY: `device` is a live fd owned above; F_GETFL/F_SETFL only read
    // and change its status flags.
    unsafe {
        let flags = libc::fcntl(device.as_raw_fd(), libc::F_GETFL);
        if flags == -1 {
            return Err(context("fcntl F_GETFL"));
        }
        if libc::fcntl(device.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(context("fcntl F_SETFL O_NONBLOCK"));
        }
    }

    let bufsize = SOCKET_BUFFER_BYTES;
    for fd in [daemon.as_raw_fd(), device.as_raw_fd()] {
        // SAFETY: `fd` is live (owned above); FD_CLOEXEC only changes its
        // descriptor flags.
        let cloexec = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            flags != -1 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) != -1
        };
        if !cloexec {
            tracing::warn!(
                "vsock fcntl FD_CLOEXEC failed on fd {fd}: {}",
                io::Error::last_os_error()
            );
        }
        for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: `fd` is live; the option value points at a c_int that
            // outlives the call.
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    opt,
                    (&raw const bufsize).cast::<libc::c_void>(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc == -1 {
                tracing::debug!(
                    "vsock setsockopt(opt={opt}, 1MiB) on fd {fd} failed: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
    Ok((daemon, device))
}

/// Lowest descriptor the daemon end of a connection is moved to. The kernel
/// hands out the lowest free number, so the rest of the process's
/// descriptors, and every number it just closed, sit below this.
const FD_FLOOR: RawFd = 1024;

/// How many starting points above the floor successive connections rotate
/// through: a number comes back only this many connections later.
const FD_ROTATION: RawFd = 1024;

/// Moves the daemon end of a connection out of the low descriptor range,
/// starting at a position that rotates with the connection's `host_port`,
/// and closes the original.
///
/// During guest boot the daemon opens and drops several short-lived vsock
/// probe connections in quick succession. On macOS the low socketpair fd
/// number was recycled immediately (`20`, `20`, `20`, ...), which let
/// Tokio/kqueue reuse the same registration slot across retries; when a
/// previous registration had not been fully torn down yet, later attempts
/// could miss both EOF and timeout wakeups. Giving each connection a number
/// the previous ones did not use avoids that reuse while keeping the socket
/// semantics unchanged.
///
/// The position is a preference, never a cap. When every descriptor from it
/// up to the soft `RLIMIT_NOFILE` is taken, the search wraps to the floor and
/// then to any free number, so connections are bounded only by the process's
/// limit. Under a limit below 2048, the floor and the rotation shrink to the
/// upper half of it. The error names the limit.
pub fn relocate_daemon_end(fd: OwnedFd, host_port: u32) -> io::Result<OwnedFd> {
    let limit = soft_nofile_limit();
    let floor = FD_FLOOR.min(limit / 2);
    let rotation = FD_ROTATION.min(limit - floor).max(1);
    let start = floor + (host_port % rotation as u32).cast_signed();

    let mut error = io::Error::from_raw_os_error(libc::EMFILE);
    for min_fd in [start, floor, 0] {
        // SAFETY: `fd` is a live OwnedFd; fcntl(F_DUPFD_CLOEXEC) only
        // allocates a new descriptor for the same open file.
        let dup_fd = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, min_fd) };
        if dup_fd >= 0 {
            // SAFETY: `dup_fd` is a fresh fd produced by the kernel; no other
            // owner exists, so `OwnedFd` takes sole ownership.
            return Ok(unsafe { OwnedFd::from_raw_fd(dup_fd) });
        }
        error = io::Error::last_os_error();
        // EMFILE: nothing is free from `min_fd` up. EINVAL: `min_fd` is at or
        // above `kern.maxfilesperproc`, which macOS enforces even when the
        // soft limit is higher.
        if !matches!(error.raw_os_error(), Some(libc::EMFILE | libc::EINVAL)) {
            break;
        }
    }
    Err(io::Error::new(
        error.kind(),
        format!("{error} (limit={limit})"),
    ))
}

/// The soft `RLIMIT_NOFILE` as a descriptor number; `RLIM_INFINITY` and
/// anything else past `RawFd::MAX` saturate. 0 when it cannot be read.
fn soft_nofile_limit() -> RawFd {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one `rlimit` through a pointer that is valid
    // for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return 0;
    }
    RawFd::try_from(limit.rlim_cur).unwrap_or(RawFd::MAX)
}

fn context(what: &str) -> io::Error {
    let err = io::Error::last_os_error();
    io::Error::new(err.kind(), format!("{what}: {err}"))
}
