//! Serial port configuration.

use crate::error::{VZError, VZResult};
use crate::shim_ffi;
use std::ffi::c_void;
use std::os::unix::io::RawFd;

/// Configuration for a serial port.
///
/// The pipes behind a [`Self::virtio_console`] port are owned by the caller:
/// the shim wraps the VZ-facing ends in `FileHandle`s that never close them.
/// Once the VM has started, the VZ helper process holds its own copies of
/// those ends, and the caller closes [`Self::guest_fds`]; keeping them open
/// leaks two fds per port and, worse, keeps the guest-output pipe from ever
/// delivering EOF to its reader.
pub struct SerialPortConfiguration {
    inner: *mut c_void,
    /// Host-side ends (`read_fd`, `write_fd`).
    fds: Option<(RawFd, RawFd)>,
    /// VZ-facing ends (`read_fd`, `write_fd`): what the guest reads input
    /// from and writes output to.
    guest_fds: Option<(RawFd, RawFd)>,
}

// SAFETY: The inner pointer is an ObjC configuration object created by the
// shim; it is not mutated concurrently.
unsafe impl Send for SerialPortConfiguration {}

impl SerialPortConfiguration {
    /// Creates a `VirtIO` console serial port configuration using pipes.
    ///
    /// This creates a serial port that appears as `hvc0` in the guest.
    /// Returns the configuration and the file descriptors for reading/writing.
    pub fn virtio_console() -> VZResult<Self> {
        // Pipes are created and owned on the Rust side; the shim only wraps
        // the VZ-facing ends in FileHandles (without closing them).
        let mut input_pipe: [libc::c_int; 2] = [0, 0];
        let mut output_pipe: [libc::c_int; 2] = [0, 0];

        // SAFETY: libc::pipe writes two valid fds on success.
        unsafe {
            if libc::pipe(input_pipe.as_mut_ptr()) != 0 {
                return Err(VZError::OperationFailed(
                    "Failed to create input pipe".to_string(),
                ));
            }
            if libc::pipe(output_pipe.as_mut_ptr()) != 0 {
                libc::close(input_pipe[0]);
                libc::close(input_pipe[1]);
                return Err(VZError::OperationFailed(
                    "Failed to create output pipe".to_string(),
                ));
            }
        }

        // VZ reads guest input from input_pipe[0] and writes guest output to
        // output_pipe[1].
        // SAFETY: both fds are valid; the shim returns a +1 handle.
        let obj = unsafe { shim_ffi::abx_serial_console_new(input_pipe[0], output_pipe[1]) };

        // Host-side ends: read guest output from output_pipe[0], write guest
        // input to input_pipe[1].
        Ok(Self {
            inner: obj,
            fds: Some((output_pipe[0], input_pipe[1])),
            guest_fds: Some((input_pipe[0], output_pipe[1])),
        })
    }

    /// Returns the VZ-facing ends (`read_fd`, `write_fd`) of the pipes: the
    /// fd VZ reads guest input from and the fd it writes guest output to.
    ///
    /// The caller owns them. Close them once the VM has started (the helper
    /// process has its own copies by then) or when the configuration is
    /// abandoned; never before `start`, which is when VZ hands them over.
    #[must_use]
    pub fn guest_fds(&self) -> Option<(RawFd, RawFd)> {
        self.guest_fds
    }

    /// Returns the file descriptor for reading output from the VM.
    #[must_use]
    pub fn read_fd(&self) -> Option<RawFd> {
        self.fds.map(|(r, _)| r)
    }

    /// Returns the file descriptor for writing input to the VM.
    #[must_use]
    pub fn write_fd(&self) -> Option<RawFd> {
        self.fds.map(|(_, w)| w)
    }

    /// Consumes the configuration and returns the raw pointer.
    ///
    /// Note: The file descriptors are NOT closed when this is called.
    /// The caller is responsible for managing them.
    #[must_use]
    pub(crate) fn into_ptr(self) -> *mut c_void {
        let ptr = self.inner;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for SerialPortConfiguration {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            // SAFETY: releasing the +1 handle returned by the shim.
            unsafe { shim_ffi::abx_object_release(self.inner.cast()) };
        }
        // Note: We don't close fds here as they may still be in use
    }
}

#[cfg(test)]
mod tests {
    use super::SerialPortConfiguration;

    /// Writes one byte into `w` and reads it back from `r`.
    fn crosses(w: libc::c_int, r: libc::c_int) -> bool {
        let mut byte = 0u8;
        // SAFETY: both fds are live pipe ends owned by the test.
        unsafe {
            libc::write(w, [7u8].as_ptr().cast(), 1) == 1
                && libc::read(r, (&raw mut byte).cast(), 1) == 1
                && byte == 7
        }
    }

    /// Pins which end of which pipe the caller keeps and which it hands to
    /// VZ: guest output written to the guest write end arrives at the host
    /// read end, and host input arrives at the guest read end. Needs no
    /// entitlement — nothing here initializes a VM.
    #[test]
    fn host_and_guest_ends_pair_up_across_the_two_pipes() {
        let port = SerialPortConfiguration::virtio_console().unwrap();
        let (host_read, host_write) = (port.read_fd().unwrap(), port.write_fd().unwrap());
        let (guest_read, guest_write) = port.guest_fds().unwrap();
        assert!(
            crosses(guest_write, host_read),
            "guest output reaches the host"
        );
        assert!(
            crosses(host_write, guest_read),
            "host input reaches the guest"
        );
        for fd in [host_read, host_write, guest_read, guest_write] {
            // SAFETY: closing fds this test owns, once each.
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
    }
}
