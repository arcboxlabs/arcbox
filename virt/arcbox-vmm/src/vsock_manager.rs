//! Host-side vsock connection manager for the HV (Hypervisor.framework) backend.
//!
//! Tracks multiple concurrent connections to the same guest port by assigning
//! a unique ephemeral host port per connection. Each connection owns its
//! internal socketpair fd via `OwnedFd`, ensuring automatic cleanup on drop.

use std::collections::HashMap;
#[cfg(test)]
use std::os::unix::io::FromRawFd;
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};

use arcbox_virtio::vsock::VsockHostConnections;

/// Unique identifier for a host↔guest vsock connection.
///
/// The vsock protocol identifies connections by the 4-tuple
/// `(src_cid, src_port, dst_cid, dst_port)`. Since host CID is always 2
/// and guest CID is always 3, the pair `(host_port, guest_port)` suffices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VsockConnectionId {
    pub host_port: u32,
    pub guest_port: u32,
}

/// Connection lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VsockConnectionState {
    /// OP_REQUEST sent, waiting for OP_RESPONSE from guest.
    Connecting,
    /// OP_RESPONSE received, data forwarding enabled.
    Connected,
}

/// A single host↔guest vsock connection.
///
/// Owns the internal end of the socketpair. When this entry is removed from
/// the manager (or the manager is dropped), `OwnedFd::drop` closes the fd.
pub struct VsockConnectionEntry {
    pub id: VsockConnectionId,
    pub internal_fd: OwnedFd,
    pub state: VsockConnectionState,
    pub guest_cid: u64,
    /// Bytes forwarded from host to guest (tracks OP_RW.fwd_cnt).
    /// The guest's virtio_transport uses this to determine whether the
    /// host has buffer space. If fwd_cnt never advances, the guest thinks
    /// the host has no room and stops delivering data to the accepted socket.
    pub tx_fwd_cnt: u32,
}

/// Manages all active host-initiated vsock connections for the HV backend.
///
/// Thread-safe: wrapped in `Arc<Mutex<>>` and shared between the daemon
/// threads (which call `allocate`) and vCPU threads (which call `poll`
/// methods via `VsockHostConnections` trait).
pub struct VsockConnectionManager {
    connections: HashMap<VsockConnectionId, VsockConnectionEntry>,
    /// Connections in `Connecting` state that need OP_REQUEST injection.
    pending: Vec<(VsockConnectionId, u64)>,
    /// Monotonically increasing counter for ephemeral host port allocation.
    next_host_port: AtomicU32,
}

impl VsockConnectionManager {
    /// Starting ephemeral port. Each connection gets the next value.
    const EPHEMERAL_PORT_BASE: u32 = 50_000;

    /// Creates a new empty connection manager.
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            pending: Vec::new(),
            next_host_port: AtomicU32::new(Self::EPHEMERAL_PORT_BASE),
        }
    }

    /// Allocates a new connection to `guest_port`, returning a unique ID.
    ///
    /// The `internal_fd` is the internal end of a socketpair; the external
    /// end was returned to the daemon caller. Ownership of `internal_fd`
    /// transfers to the manager — it will be closed automatically when the
    /// connection is removed.
    pub fn allocate(
        &mut self,
        guest_port: u32,
        guest_cid: u64,
        internal_fd: OwnedFd,
    ) -> VsockConnectionId {
        let host_port = self.next_host_port.fetch_add(1, Ordering::Relaxed);
        let id = VsockConnectionId {
            host_port,
            guest_port,
        };
        let entry = VsockConnectionEntry {
            id,
            internal_fd,
            state: VsockConnectionState::Connecting,
            guest_cid,
            tx_fwd_cnt: 0,
        };
        self.connections.insert(id, entry);
        self.pending.push((id, guest_cid));
        id
    }

    /// Drains pending connections that need OP_REQUEST injection.
    ///
    /// Returns `(id, guest_cid)` pairs. The caller should build and inject
    /// an OP_REQUEST packet for each, then call `re_queue_pending` for any
    /// that failed injection.
    pub fn drain_pending(&mut self) -> Vec<(VsockConnectionId, u64)> {
        std::mem::take(&mut self.pending)
    }

    /// Re-queues a connection for deferred OP_REQUEST injection.
    pub fn re_queue_pending(&mut self, id: VsockConnectionId, guest_cid: u64) {
        self.pending.push((id, guest_cid));
    }

    /// Returns a snapshot of all connected (id, raw_fd) pairs for polling.
    ///
    /// The caller uses these to `libc::read` from each fd and inject OP_RW
    /// packets into the guest RX queue.
    pub fn connected_fds(&self) -> Vec<(VsockConnectionId, RawFd)> {
        self.connections
            .values()
            .filter(|e| e.state == VsockConnectionState::Connected)
            .map(|e| (e.id, e.internal_fd.as_raw_fd()))
            .collect()
    }

    /// Advances the tx_fwd_cnt for a connection and returns the NEW value.
    /// Called after injecting OP_RW data into the guest RX queue.
    pub fn advance_tx_fwd_cnt(&mut self, id: &VsockConnectionId, bytes: u32) -> u32 {
        if let Some(entry) = self.connections.get_mut(id) {
            entry.tx_fwd_cnt = entry.tx_fwd_cnt.wrapping_add(bytes);
            entry.tx_fwd_cnt
        } else {
            0
        }
    }

    /// Returns the current tx_fwd_cnt for a connection.
    pub fn tx_fwd_cnt(&self, id: &VsockConnectionId) -> u32 {
        self.connections.get(id).map_or(0, |e| e.tx_fwd_cnt)
    }

    /// Returns the number of active connections.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.connections.len()
    }
}

impl VsockHostConnections for VsockConnectionManager {
    fn fd_for(&self, guest_port: u32, host_port: u32) -> Option<RawFd> {
        let id = VsockConnectionId {
            host_port,
            guest_port,
        };
        self.connections
            .get(&id)
            .filter(|e| e.state == VsockConnectionState::Connected)
            .map(|e| e.internal_fd.as_raw_fd())
    }

    fn mark_connected(&mut self, guest_port: u32, host_port: u32) {
        let id = VsockConnectionId {
            host_port,
            guest_port,
        };
        if let Some(entry) = self.connections.get_mut(&id) {
            entry.state = VsockConnectionState::Connected;
            tracing::debug!("VsockConnectionManager: connection {:?} now Connected", id,);
        } else {
            tracing::warn!(
                "VsockConnectionManager: mark_connected for unknown connection \
                 guest_port={} host_port={}",
                guest_port,
                host_port,
            );
        }
    }

    fn remove_connection(&mut self, guest_port: u32, host_port: u32) {
        let id = VsockConnectionId {
            host_port,
            guest_port,
        };
        if self.connections.remove(&id).is_some() {
            // OwnedFd is dropped here, closing the internal socketpair fd.
            tracing::info!(
                "VsockConnectionManager: removed connection guest_port={} host_port={} — \
                 fd closed, daemon will retry",
                guest_port,
                host_port,
            );
        }
        // Also remove from pending if it was still queued.
        self.pending.retain(|(pid, _)| *pid != id);
    }
}

impl Default for VsockConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_socketpair() -> (OwnedFd, OwnedFd) {
        let mut fds: [libc::c_int; 2] = [0; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn allocate_unique_host_ports() {
        let mut mgr = VsockConnectionManager::new();
        let (_, internal1) = make_socketpair();
        let (_, internal2) = make_socketpair();

        let id1 = mgr.allocate(1024, 3, internal1);
        let id2 = mgr.allocate(1024, 3, internal2);

        assert_ne!(id1.host_port, id2.host_port);
        assert_eq!(id1.guest_port, 1024);
        assert_eq!(id2.guest_port, 1024);
        assert_eq!(mgr.len(), 2);
    }

    #[test]
    fn connected_fds_only_returns_connected() {
        let mut mgr = VsockConnectionManager::new();
        let (_, internal1) = make_socketpair();
        let (_, internal2) = make_socketpair();

        let id1 = mgr.allocate(1024, 3, internal1);
        let _id2 = mgr.allocate(1024, 3, internal2);

        assert!(mgr.connected_fds().is_empty());

        mgr.mark_connected(id1.guest_port, id1.host_port);
        let fds = mgr.connected_fds();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].0, id1);
    }

    #[test]
    fn remove_closes_fd() {
        let mut mgr = VsockConnectionManager::new();
        let (_, internal) = make_socketpair();
        let fd_raw = internal.as_raw_fd();
        let id = mgr.allocate(1024, 3, internal);

        mgr.mark_connected(id.guest_port, id.host_port);
        assert!(mgr.fd_for(1024, id.host_port).is_some());

        mgr.remove_connection(id.guest_port, id.host_port);
        assert!(mgr.fd_for(1024, id.host_port).is_none());
        assert_eq!(mgr.len(), 0);

        // Verify fd is actually closed (write should fail with EBADF).
        let ret = unsafe { libc::fcntl(fd_raw, libc::F_GETFD) };
        assert_eq!(ret, -1);
    }

    #[test]
    fn drain_pending() {
        let mut mgr = VsockConnectionManager::new();
        let (_, internal) = make_socketpair();
        let id = mgr.allocate(1024, 3, internal);

        let pending = mgr.drain_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, id);
        assert!(mgr.drain_pending().is_empty());
    }
}
