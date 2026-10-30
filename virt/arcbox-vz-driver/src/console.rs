//! The console (`hvc0`) and agent-log (`hvc1`) channels over VZ's pipes.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use arcbox_vm_driver::{Console, Result};
use arcbox_vz::SerialPortConfiguration;
use async_trait::async_trait;

use crate::driver_error;

/// One virtio-console port: the host's read end of the guest's output, and
/// the VZ-facing ends until the VM has started.
pub struct PipeConsole {
    /// Guest output; non-blocking for life.
    output: OwnedFd,
    /// Guest input; nothing writes it yet, but VZ needs the pipe.
    _input: OwnedFd,
    /// The ends VZ hands to its helper at start. Closed by
    /// [`PipeConsole::release_guest_ends`] once the VM runs, so the output
    /// pipe can reach EOF when the helper exits.
    guest_ends: Option<(OwnedFd, OwnedFd)>,
}

impl PipeConsole {
    /// A new port and the VZ configuration that serves it.
    pub fn new(label: &str) -> Result<(Self, SerialPortConfiguration)> {
        let port = SerialPortConfiguration::virtio_console()
            .map_err(|e| driver_error(format!("{label} console"), e))?;
        let (Some(output), Some(input), Some((guest_read, guest_write))) =
            (port.read_fd(), port.write_fd(), port.guest_fds())
        else {
            return Err(driver_error(
                format!("{label} console"),
                std::io::Error::other("pipe console without descriptors"),
            ));
        };
        // `virtio_console` created these four pipe ends and hands their
        // ownership to the caller (see `SerialPortConfiguration`).
        let own = |fd: RawFd| {
            // SAFETY: `fd` is one of the four ends above, owned by nobody else.
            unsafe { OwnedFd::from_raw_fd(fd) }
        };
        let console = Self {
            output: own(output),
            _input: own(input),
            guest_ends: Some((own(guest_read), own(guest_write))),
        };
        set_nonblocking(console.output.as_raw_fd())?;
        Ok((console, port))
    }

    /// Closes this process's copies of the VZ-facing ends; call once the VM
    /// has started and VZ's helper holds its own.
    pub fn release_guest_ends(&mut self) {
        self.guest_ends = None;
    }
}

#[async_trait]
impl Console for PipeConsole {
    async fn read_output(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; max_bytes];
        let mut filled = 0;
        let mut pipe = std::fs::File::from(self.output.try_clone()?);
        while filled < max_bytes {
            match pipe.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn dup_reader(&self) -> Result<OwnedFd> {
        Ok(self.output.try_clone()?)
    }
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: fcntl get on a live descriptor this console owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: fcntl set on the same descriptor, adding O_NONBLOCK.
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
