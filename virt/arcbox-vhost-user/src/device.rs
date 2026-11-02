//! The interface a device implements to be served over vhost-user.

use crate::error::{Error, Result};
use crate::event_loop::Registrar;
use crate::protocol::ProtocolFeatures;
use crate::vring::Vring;

/// A virtio device whose data plane runs in this process while QEMU owns the
/// transport (PCI, interrupts, guest memory).
///
/// Threading: the back-end calls every method with one lock held, from two
/// threads — the control thread (front-end messages: features, config,
/// reset) and the worker thread (queue kicks and the device's own events).
/// A method never runs concurrently with another, so `&mut self` needs no
/// further synchronization; a device that wants work done elsewhere hands it
/// to its own threads and wakes the worker with a [`crate::Waker`].
///
/// A handler must finish every chain it pops before it returns: the control
/// thread may stop a ring (`GET_VRING_BASE`) the moment the lock is released,
/// and reports the avail cursor as everything consumed.
pub trait VhostUserDevice: Send + 'static {
    /// Number of virtqueues (`GET_QUEUE_NUM`).
    fn num_queues(&self) -> usize;

    /// Largest queue size the device accepts in `SET_VRING_NUM`.
    fn max_queue_size(&self) -> u16 {
        1024
    }

    /// Virtio feature bits the device offers, including `VIRTIO_F_VERSION_1`
    /// and `VIRTIO_RING_F_EVENT_IDX` if wanted. The back-end adds
    /// `VHOST_USER_F_PROTOCOL_FEATURES` and strips the ring layouts
    /// `SplitQueue` does not implement (`INDIRECT_DESC`, `RING_PACKED`).
    fn features(&self) -> u64;

    /// Protocol features beyond the ones the back-end always offers
    /// (`MQ`, `REPLY_ACK`, `RESET_DEVICE`). A device with a config space
    /// returns [`ProtocolFeatures::CONFIG`].
    fn protocol_features(&self) -> ProtocolFeatures {
        ProtocolFeatures::empty()
    }

    /// The driver's accepted features (`SET_FEATURES`), without the vhost-user
    /// transport bit.
    fn set_features(&mut self, features: u64) {
        let _ = features;
    }

    /// Reads `data.len()` bytes of config space at `offset` (`GET_CONFIG`).
    fn read_config(&self, offset: u32, data: &mut [u8]) -> Result<()> {
        let _ = (offset, data);
        Err(Error::Unsupported("device has no config space".into()))
    }

    /// Writes config space (`SET_CONFIG`).
    fn write_config(&mut self, offset: u32, data: &[u8]) -> Result<()> {
        let _ = (offset, data);
        Err(Error::Unsupported(
            "device config space is read-only".into(),
        ))
    }

    /// Called once per connection before the back-end handles the first
    /// message; register descriptors and create wakers here. Every
    /// registration ends with the connection.
    fn activate(&mut self, registrar: &Registrar) -> Result<()> {
        let _ = registrar;
        Ok(())
    }

    /// The guest kicked `queue`, or the ring was just started or enabled.
    /// Only called while the ring [`Vring::is_running`].
    fn process_queue(&mut self, queue: u16, vrings: &mut Vrings) -> Result<()>;

    /// A descriptor or waker registered through the [`Registrar`] fired.
    fn handle_event(&mut self, token: u64, vrings: &mut Vrings) -> Result<()> {
        let _ = (token, vrings);
        Ok(())
    }

    /// The front-end reset the device or went away. Drop all per-connection
    /// state, including any guest memory handle taken from a [`Vring`]: a
    /// handle keeps its mapping alive, so one kept past this point pins guest
    /// RAM the front-end no longer uses.
    fn reset(&mut self) {}
}

/// The device's queues, plus the event-loop registrar.
#[derive(Debug)]
pub struct Vrings {
    rings: Vec<Vring>,
    registrar: Registrar,
}

impl Vrings {
    pub(crate) fn new(count: usize, max_size: u16, registrar: Registrar) -> Self {
        Self {
            rings: (0..count).map(|i| Vring::new(i as u16, max_size)).collect(),
            registrar,
        }
    }

    /// Number of queues.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rings.len()
    }

    /// Whether the device has no queues.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }

    /// One queue.
    #[must_use]
    pub fn get(&self, index: u16) -> Option<&Vring> {
        self.rings.get(usize::from(index))
    }

    /// One queue, mutably.
    pub fn get_mut(&mut self, index: u16) -> Option<&mut Vring> {
        self.rings.get_mut(usize::from(index))
    }

    /// Every queue.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Vring> {
        self.rings.iter_mut()
    }

    /// Registers device descriptors and wakers with the event loop.
    #[must_use]
    pub const fn registrar(&self) -> &Registrar {
        &self.registrar
    }
}
