//! The virtio-mmio transport state a checkpoint saves per device.

use serde::{Deserialize, Serialize};

use crate::device::{MAX_VIRTQUEUES, VirtioMmioState};
use crate::error::{Result, VmmError};

/// One virtqueue's transport registers.
#[derive(Serialize, Deserialize)]
struct QueueState {
    index: usize,
    num: u16,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
}

/// One virtio-mmio device's transport state and host-owned configuration.
/// Everything else a device keeps between requests lives in guest RAM: the
/// queue workers derive their avail cursor from the used ring.
#[derive(Serialize, Deserialize)]
pub(super) struct DeviceState {
    pub(super) mmio_base: u64,
    device_id: u32,
    device_features: u64,
    pub(super) driver_features: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    queue_sel: u32,
    pub(super) status: u8,
    interrupt_status: u32,
    config_generation: u32,
    shm_sel: u32,
    queues: Vec<QueueState>,
    /// `VirtioDevice::save_config`: the configuration-space fields the
    /// host owns, such as a NIC's MAC.
    pub(super) config: Vec<u8>,
}

impl DeviceState {
    pub(super) fn capture(mmio_base: u64, s: &VirtioMmioState, config: Vec<u8>) -> Self {
        Self {
            mmio_base,
            device_id: s.device_id,
            device_features: s.device_features,
            driver_features: s.driver_features,
            device_features_sel: s.device_features_sel,
            driver_features_sel: s.driver_features_sel,
            queue_sel: s.queue_sel,
            status: s.status,
            interrupt_status: s.interrupt_status,
            config_generation: s.config_generation,
            shm_sel: s.shm_sel,
            queues: (0..MAX_VIRTQUEUES)
                .filter(|&q| s.queue_num[q] != 0 || s.queue_ready[q] || s.queue_desc[q] != 0)
                .map(|q| QueueState {
                    index: q,
                    num: s.queue_num[q],
                    ready: s.queue_ready[q],
                    desc: s.queue_desc[q],
                    driver: s.queue_driver[q],
                    device: s.queue_device[q],
                })
                .collect(),
            config,
        }
    }

    pub(super) fn apply(&self, s: &mut VirtioMmioState) -> Result<()> {
        if s.device_id != self.device_id {
            return Err(VmmError::invalid_state(format!(
                "device at {:#x} is type {}, the checkpoint has type {}",
                self.mmio_base, s.device_id, self.device_id
            )));
        }
        s.device_features = self.device_features;
        s.driver_features = self.driver_features;
        s.device_features_sel = self.device_features_sel;
        s.driver_features_sel = self.driver_features_sel;
        s.queue_sel = self.queue_sel;
        s.status = self.status;
        s.interrupt_status = self.interrupt_status;
        s.config_generation = self.config_generation;
        s.shm_sel = self.shm_sel;
        for q in &self.queues {
            if q.index >= MAX_VIRTQUEUES {
                return Err(VmmError::invalid_state(format!(
                    "queue {} out of range",
                    q.index
                )));
            }
            s.queue_num[q.index] = q.num;
            s.queue_ready[q.index] = q.ready;
            s.queue_desc[q.index] = q.desc;
            s.queue_driver[q.index] = q.driver;
            s.queue_device[q.index] = q.device;
        }
        Ok(())
    }
}
