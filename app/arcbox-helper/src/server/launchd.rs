//! launchd socket activation via `launch_activate_socket`.
//!
//! When the helper is registered as a LaunchDaemon with a `Sockets`
//! configuration, launchd pre-creates the Unix socket and passes the
//! file descriptor to the process on start. This avoids the helper
//! needing to manage socket lifecycle or permissions.

use std::os::unix::io::FromRawFd;

/// The socket name in the launchd plist `Sockets` dictionary.
const LAUNCHD_SOCKET_NAME: &str = "helper";

unsafe extern "C" {
    fn launch_activate_socket(
        name: *const libc::c_char,
        fds: *mut *mut libc::c_int,
        cnt: *mut libc::size_t,
    ) -> libc::c_int;
}

/// Gets a Unix listener from launchd socket activation.
///
/// Returns `Some` when running under launchd with a `Sockets` config,
/// `None` when started manually (dev mode).
pub fn listener() -> Option<tokio::net::UnixListener> {
    let name = std::ffi::CString::new(LAUNCHD_SOCKET_NAME).ok()?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut cnt: libc::size_t = 0;

    // SAFETY: launch_activate_socket is a well-defined macOS API. We pass
    // valid pointers and check the return value before using fds.
    let ret = unsafe { launch_activate_socket(name.as_ptr(), &raw mut fds, &raw mut cnt) };

    if ret != 0 || cnt == 0 || fds.is_null() {
        return None;
    }

    // SAFETY: fds points to a valid array of cnt ints allocated by the system.
    let fd = unsafe { *fds };

    // SAFETY: fds was allocated by launch_activate_socket; we must free it.
    unsafe { libc::free(fds.cast::<libc::c_void>()) };

    // SAFETY: fd is a valid listening socket from launchd.
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true).ok()?;
    tokio::net::UnixListener::from_std(std_listener).ok()
}
