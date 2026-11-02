//! Pieces the QEMU probes share.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arcbox_vhost_user::{ProtocolFeatures, VhostUserDevice, Vrings};

/// A socketpair for one vhost-user device: the back-end's end, and the end
/// QEMU's `-chardev socket,fd=` inherits (the only one that survives `exec`).
pub fn chardev_pair() -> std::io::Result<(UnixStream, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair writes two descriptors into `fds`.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors, sole owner of each.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: F_SETFD on an owned descriptor; only ours is close-on-exec.
    unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok((UnixStream::from(ours), theirs))
}

/// vhost-user-vsock with a config space and nothing else: enough for QEMU
/// to realize the device and for the guest to get a vsock transport.
pub struct VsockConfigOnly {
    pub cid: u64,
    /// Set once the guest posts RX buffers.
    pub rx_posted: Arc<AtomicBool>,
}

impl VhostUserDevice for VsockConfigOnly {
    fn num_queues(&self) -> usize {
        2 // RX, TX; QEMU emulates the event queue itself
    }

    fn features(&self) -> u64 {
        1 << 32 // VIRTIO_F_VERSION_1
    }

    fn protocol_features(&self) -> ProtocolFeatures {
        ProtocolFeatures::CONFIG
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) -> arcbox_vhost_user::Result<()> {
        // struct virtio_vsock_config { le64 guest_cid; }
        let config = self.cid.to_le_bytes();
        let src = config
            .get(offset as usize..offset as usize + data.len())
            .ok_or_else(|| arcbox_vhost_user::Error::Device(format!("config read at {offset}")))?;
        data.copy_from_slice(src);
        Ok(())
    }

    fn process_queue(&mut self, queue: u16, vrings: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        if queue == 0 {
            let posted = vrings
                .get_mut(0)
                .and_then(|v| v.queue_mut())
                .is_some_and(|q| q.has_avail());
            self.rx_posted.fetch_or(posted, Ordering::SeqCst);
        }
        Ok(())
    }
}
