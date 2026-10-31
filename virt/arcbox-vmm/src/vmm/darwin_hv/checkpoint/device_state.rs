//! The virtio-mmio transport state a checkpoint saves per device.

use serde::{Deserialize, Serialize};

use crate::device::{MAX_VIRTQUEUES, VirtioMmioState};

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

/// One virtio-mmio device's transport state. Everything else a device
/// keeps between requests lives in guest RAM: the queue workers derive
/// their avail cursor from the used ring.
#[derive(Serialize, Deserialize)]
pub(super) struct DeviceState {
    mmio_base: u64,
    device_id: u32,
    device_features: u64,
    driver_features: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    queue_sel: u32,
    status: u8,
    interrupt_status: u32,
    config_generation: u32,
    shm_sel: u32,
    queues: Vec<QueueState>,
}

impl DeviceState {
    pub(super) fn capture(mmio_base: u64, s: &VirtioMmioState) -> Self {
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
        }
    }
}
