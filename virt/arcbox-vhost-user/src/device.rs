//! The interface a device implements to be served over vhost-user.

use crate::error::{Error, Result};
use crate::protocol::ProtocolFeatures;

/// A virtio device whose data plane runs in this process while QEMU owns the
/// transport (PCI, interrupts, guest memory).
pub trait VhostUserDevice: Send + 'static {
    /// Number of virtqueues (`GET_QUEUE_NUM`).
    fn num_queues(&self) -> usize;

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

    /// The front-end reset the device or went away. Drop all per-connection
    /// state.
    fn reset(&mut self) {}
}
