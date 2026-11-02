//! The vsock data plane as a vhost-user device.
//!
//! The guest's TX packets and host→guest injection run through the same
//! [`VirtioVsock`] code the HV backend uses, against rings QEMU set up in
//! shared guest RAM. One back-end worker thread runs every pass; what wakes
//! it is the guest's kicks on either queue, the connection manager's
//! doorbell (a host connect, a reopened credit window), and readability of
//! every connected host socket.

use std::collections::HashSet;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arcbox_vhost_user::{ProtocolFeatures, Registrar, Trigger, VhostUserDevice, Vrings};
use arcbox_virtio_core::{DeviceCtx, VirtioDevice};

use crate::device::{VirtioVsock, VsockConfig};
use crate::manager::{VsockConnectionId, VsockConnectionManager};

/// Queue 0 carries host→guest packets.
const RX: u16 = 0;
/// Queue 1 carries guest→host packets. QEMU emulates the event queue.
const TX: u16 = 1;

/// The manager rang the doorbell.
const DOORBELL: u64 = 0;
/// A connected host socket became readable or reached EOF.
const READABLE: u64 = 1;

pub(super) struct VsockDevice {
    guest_cid: u64,
    connections: Arc<Mutex<VsockConnectionManager>>,
    vsock: VirtioVsock,
    event_idx: bool,
    /// Connections whose host socket the event loop watches. Closing the
    /// socket (connection removal) drops its registration, so this set only
    /// has to forget the connection.
    watched: HashSet<VsockConnectionId>,
}

impl VsockDevice {
    pub(super) fn new(guest_cid: u64, connections: Arc<Mutex<VsockConnectionManager>>) -> Self {
        Self {
            guest_cid,
            vsock: bound_vsock(guest_cid, &connections),
            connections,
            event_idx: false,
            watched: HashSet::new(),
        }
    }

    /// Runs the data plane until it has nothing left to do without the
    /// guest: guest→host packets first, so a handshake or credit update in
    /// them lets the injection that follows go further, then host→guest
    /// injection. Both rings are re-armed before the pass ends; a kick that
    /// raced the re-arm means another pass.
    fn service(&mut self, vrings: &mut Vrings) {
        let memory = vrings
            .get(RX)
            .and_then(arcbox_vhost_user::Vring::memory)
            .or_else(|| vrings.get(TX).and_then(arcbox_vhost_user::Vring::memory));
        let Some(mem) = memory else { return };
        // The injection path reads guest memory through the device context.
        // It holds the mapping only for this pass: the front-end may replace
        // the table (guest reboot) before the next one.
        self.vsock.bind_ctx(DeviceCtx {
            mem,
            raise_irq: Arc::new(|_| {}),
        });
        loop {
            let tx_again = self.drain_tx(vrings);
            let rx_again = self.inject_rx(vrings);
            if !tx_again && !rx_again {
                break;
            }
        }
        self.vsock.unbind_ctx();
        self.watch_connections(vrings.registrar());
    }

    /// Consumes every guest→host packet. Returns whether the guest posted
    /// more after the queue was re-armed.
    fn drain_tx(&mut self, vrings: &mut Vrings) -> bool {
        let Some(tx) = vrings.get_mut(TX) else {
            return false;
        };
        let (Some(config), Some(mem)) = (tx.queue_config(), tx.memory()) else {
            return false;
        };
        let used = config.used_addr as usize + 2;
        let old_used = mem.read_u16(used);
        // SAFETY: the slice covers the guest RAM `mem` maps and is used only
        // for this call, which accesses it under the virtio descriptor
        // ownership rules; `mem` keeps the mapping alive meanwhile.
        let Some(ram) = (unsafe { mem.slice_mut(mem.gpa_base(), mem.len()) }) else {
            return false;
        };
        let completions = match VirtioDevice::process_queue(&mut self.vsock, TX, ram, &config) {
            Ok(completions) => completions,
            Err(e) => {
                tracing::warn!("vhost-user vsock TX: {e}");
                Vec::new()
            }
        };
        // TX completes every chain it pops, so the avail cursor is used.idx.
        let new_used = mem.read_u16(used);
        tx.set_last_avail_idx(new_used);
        if !completions.is_empty() {
            // A packet may have queued host→guest work (a credit update, a
            // RESPONSE); make sure injection visits those connections.
            let mut manager = lock(&self.connections);
            for id in manager.connections_with_pending_rx() {
                manager.backend_rxq.push_back(id);
            }
        }
        let Some(queue) = tx.queue_mut() else {
            return false;
        };
        if queue.notify_needed(old_used, new_used) {
            tx.signal_used();
        }
        tx.queue_mut().is_some_and(|q| q.enable_notification())
    }

    /// Injects queued host→guest packets into the buffers the guest posted.
    /// Returns whether work is left that a buffer posted during the re-arm
    /// can take.
    fn inject_rx(&mut self, vrings: &mut Vrings) -> bool {
        let Some(rx) = vrings.get_mut(RX) else {
            return false;
        };
        let (Some(config), Some(mem)) = (rx.queue_config(), rx.memory()) else {
            return false;
        };
        let round = self.vsock.poll_rx_injection(&config, None, self.event_idx);
        // Injection consumes one avail entry per used entry.
        rx.set_last_avail_idx(mem.read_u16(config.used_addr as usize + 2));
        if round.raise {
            rx.signal_used();
        }
        // Work stuck on a missing buffer waits for the guest's next RX kick,
        // which the re-arm asks for; a buffer posted meanwhile means go on.
        let starved = !lock(&self.connections).backend_rxq.is_empty();
        let posted = rx.queue_mut().is_some_and(|q| q.enable_notification());
        if starved && !posted {
            tracing::debug!("vhost-user vsock RX: no guest buffer; parked until the guest posts");
        }
        starved && posted
    }

    /// Watches the host socket of every connection that finished its
    /// handshake. Edge-triggered: a pass may leave data unread when the guest
    /// has no buffer or no credit, and the guest's kick or credit update
    /// wakes the device for it; a level registration would spin meanwhile.
    fn watch_connections(&mut self, registrar: &Registrar) {
        let connected: Vec<(VsockConnectionId, RawFd)> = lock(&self.connections).connected_fds();
        self.watched
            .retain(|id| connected.iter().any(|(live, _)| live == id));
        for (id, fd) in connected {
            if !self.watched.insert(id) {
                continue;
            }
            // SAFETY: the manager owns `fd` and closes it only on removal,
            // which happens on this thread; the registration call does not
            // outlive this borrow.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            if let Err(e) = registrar.watch_readable(fd, READABLE, Trigger::Edge) {
                tracing::warn!("vhost-user vsock: cannot watch {id:?}: {e}");
                self.watched.remove(&id);
            }
        }
    }
}

impl VhostUserDevice for VsockDevice {
    fn num_queues(&self) -> usize {
        2
    }

    fn features(&self) -> u64 {
        self.vsock.features()
    }

    fn protocol_features(&self) -> ProtocolFeatures {
        // vhost-user-vsock-pci reads the CID at realize time.
        ProtocolFeatures::CONFIG
    }

    fn set_features(&mut self, features: u64) {
        self.vsock.ack_features(features);
        self.event_idx = features & arcbox_virtio_core::queue::VIRTIO_F_EVENT_IDX != 0;
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) -> arcbox_vhost_user::Result<()> {
        // struct virtio_vsock_config { le64 guest_cid; }
        let config = self.guest_cid.to_le_bytes();
        let start = offset as usize;
        let src = config
            .get(start..start.saturating_add(data.len()))
            .ok_or_else(|| {
                arcbox_vhost_user::Error::Device(format!(
                    "vsock config read of {} bytes at offset {offset}",
                    data.len()
                ))
            })?;
        data.copy_from_slice(src);
        Ok(())
    }

    fn activate(&mut self, registrar: &Registrar) -> arcbox_vhost_user::Result<()> {
        let waker = registrar.waker(DOORBELL)?;
        lock(&self.connections).set_doorbell(Arc::new(move || {
            if let Err(e) = waker.wake() {
                tracing::warn!("vhost-user vsock doorbell: {e}");
            }
        }));
        Ok(())
    }

    fn process_queue(&mut self, _queue: u16, vrings: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        self.service(vrings);
        Ok(())
    }

    fn handle_event(&mut self, _token: u64, vrings: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        self.service(vrings);
        Ok(())
    }

    /// The guest reset the device (reboot, driver reload) or QEMU went away:
    /// every guest socket is gone, so every host connection ends too. The
    /// daemon sees EOF on its end and reconnects.
    fn reset(&mut self) {
        lock(&self.connections).remove_all();
        self.watched.clear();
        self.event_idx = false;
        self.vsock = bound_vsock(self.guest_cid, &self.connections);
    }
}

fn bound_vsock(guest_cid: u64, connections: &Arc<Mutex<VsockConnectionManager>>) -> VirtioVsock {
    let mut vsock = VirtioVsock::new(VsockConfig { guest_cid });
    vsock.bind_connection_manager(Arc::clone(connections));
    vsock
}

pub(super) fn lock(
    connections: &Mutex<VsockConnectionManager>,
) -> MutexGuard<'_, VsockConnectionManager> {
    connections.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::*;
    use crate::manager::host_socketpair;

    const CID: u64 = 0x0102_0304_0506_0708;

    fn device() -> (VsockDevice, Arc<Mutex<VsockConnectionManager>>) {
        let connections = Arc::new(Mutex::new(VsockConnectionManager::new()));
        (VsockDevice::new(CID, Arc::clone(&connections)), connections)
    }

    #[test]
    fn config_space_is_the_little_endian_cid() {
        let (device, _) = device();
        let mut whole = [0u8; 8];
        device.read_config(0, &mut whole).unwrap();
        assert_eq!(whole, [8, 7, 6, 5, 4, 3, 2, 1]);
        // QEMU may read the two halves separately.
        let mut high = [0u8; 4];
        device.read_config(4, &mut high).unwrap();
        assert_eq!(high, [4, 3, 2, 1]);
        assert!(device.read_config(6, &mut high).is_err());
        assert!(device.read_config(8, &mut [0u8; 1]).is_err());
    }

    #[test]
    fn reset_ends_every_host_connection() {
        let (mut device, connections) = device();
        let mut hosts = Vec::new();
        for port in [1024, 2375] {
            let (host, internal) = host_socketpair().unwrap();
            lock(&connections).allocate(port, CID, internal);
            let host = UnixStream::from(host);
            // Before the reset: macOS refuses `setsockopt` once the peer
            // has gone.
            host.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            hosts.push(host);
        }
        device.reset();
        assert!(lock(&connections).is_empty());
        for mut host in hosts {
            assert_eq!(host.read(&mut [0u8; 8]).unwrap(), 0, "host end reads EOF");
        }
    }
}
