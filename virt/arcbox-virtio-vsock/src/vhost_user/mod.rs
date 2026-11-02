//! virtio-vsock for QEMU, served from this process over vhost-user.
//!
//! QEMU runs `vhost-user-vsock-pci` and owns its transport and event queue;
//! this back-end owns RX and TX. Every guest port is dialled from the host,
//! as on the HV backend: [`VhostUserVsock::connect_vsock`] returns one end of
//! a socketpair the device relays to the guest, so callers such as
//! `AgentClient` take it the way they take an HV fd.
//!
//! ```text
//! socketpair(AF_UNIX, SOCK_STREAM) → (ours, theirs)
//! qemu ... -object memory-backend-shm,id=mem,size=<RAM>,share=on \
//!          -machine virt,memory-backend=mem \
//!          -chardev socket,id=vsock0,fd=<theirs> \
//!          -device vhost-user-vsock-pci,chardev=vsock0
//! let vsock = VhostUserVsock::spawn(cid, ours)?;
//! let fd = vsock.connect_vsock(1024)?;
//! ```
//!
//! A guest reset (reboot, driver reload) ends every connection: the caller
//! reads EOF and connects again, as after an HV guest restart.

mod device;

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use arcbox_vhost_user::{Backend, BackendHandle};

use crate::manager::{VsockConnectionManager, host_socketpair, relocate_daemon_end};
use device::{VsockDevice, lock};

/// A vsock device serving one QEMU front-end on its own threads.
pub struct VhostUserVsock {
    guest_cid: u64,
    connections: Arc<Mutex<VsockConnectionManager>>,
    backend: BackendHandle<VsockDevice>,
}

impl VhostUserVsock {
    /// Starts serving the front-end on `stream`, the back-end's end of the
    /// socketpair QEMU's `-chardev socket,fd=N` holds the other end of.
    /// `guest_cid` is the CID the guest reads from the config space.
    ///
    /// QEMU blocks in device initialization until the back-end answers, so
    /// call this before or right after spawning QEMU.
    pub fn spawn(guest_cid: u64, stream: UnixStream) -> io::Result<Self> {
        let connections = Arc::new(Mutex::new(VsockConnectionManager::new()));
        let device = VsockDevice::new(guest_cid, Arc::clone(&connections));
        let backend = Backend::new("vhost-vsock", device).spawn(stream)?;
        Ok(Self {
            guest_cid,
            connections,
            backend,
        })
    }

    /// Connects to `port` in the guest and returns the host end.
    ///
    /// Returns as soon as the request is queued; the guest answers once it
    /// services the queue. A refused or reset connection reads as EOF, a
    /// guest that never answers as a read that does not complete, so
    /// callers bound their first read as they do on HV.
    pub fn connect_vsock(&self, port: u32) -> io::Result<OwnedFd> {
        if self.backend.is_finished() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "vhost-user vsock back-end has stopped",
            ));
        }
        let (host, internal) = host_socketpair()?;
        let (id, _injected) = lock(&self.connections).allocate(port, self.guest_cid, internal);
        let host = relocate_daemon_end(host, id.host_port)
            .inspect_err(|_| lock(&self.connections).remove(&id))?;
        tracing::debug!(
            "vhost-user vsock connect: guest_port={port} host_port={} fd={}",
            id.host_port,
            host.as_raw_fd()
        );
        Ok(host)
    }

    /// The guest CID.
    #[must_use]
    pub const fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Open host connections, including ones still waiting for the guest.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        lock(&self.connections).len()
    }

    /// Whether QEMU has disconnected.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.backend.is_finished()
    }

    /// Disconnects from QEMU, which stops the device in the guest, and waits
    /// for the back-end to finish.
    pub fn stop(self) -> arcbox_vhost_user::Result<()> {
        self.backend.stop().map(drop)
    }

    /// Waits for QEMU to disconnect.
    pub fn join(self) -> arcbox_vhost_user::Result<()> {
        self.backend.join().map(drop)
    }
}
