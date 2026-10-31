//! Bringing devices back after a checkpoint restore.
//!
//! The restore writes each device's transport state back directly instead
//! of replaying the guest's status writes, so the DRIVER_OK side effects
//! that live outside the device object run from here.

use super::*;

impl DeviceManager {
    /// Spawns the net-io worker for a primary VirtioNet that the guest had
    /// already driven to DRIVER_OK.
    pub fn resume_net_rx_worker(&self) {
        let Some(id) = self.primary_net_device_id else {
            return;
        };
        let Some(mmio) = self.get_mmio_state(id) else {
            return;
        };
        let active = mmio
            .read()
            .is_ok_and(|s| s.status & DeviceStatus::DRIVER_OK != 0);
        if active {
            self.maybe_spawn_net_rx_worker(id, &mmio);
        }
    }

    /// Tells the guest that every vsock connection is gone: the host ends
    /// of the connections a checkpoint captured do not exist in this
    /// process. Posts TRANSPORT_RESET on the event queue (2) and raises the
    /// used-buffer interrupt. Returns whether the event was published.
    pub fn reset_vsock_transport(&self) -> bool {
        let Some(vsock) = self.vsock.as_ref() else {
            return false;
        };
        let Some(device) = self
            .devices
            .values()
            .find(|d| d.info.device_type == DeviceType::VirtioVsock)
        else {
            return false;
        };
        let Some(mmio) = device.mmio_state.as_ref() else {
            return false;
        };
        let qcfg = {
            let Ok(s) = mmio.read() else {
                return false;
            };
            if s.status & DeviceStatus::DRIVER_OK == 0 {
                return false;
            }
            let q = 2;
            QueueConfig {
                desc_addr: s.queue_desc[q],
                avail_addr: s.queue_driver[q],
                used_addr: s.queue_device[q],
                size: s.queue_num[q],
                ready: s.queue_ready[q],
                gpa_base: self.guest_ram_gpa,
            }
        };
        let posted = vsock
            .lock()
            .is_ok_and(|mut dev| dev.post_transport_reset(&qcfg));
        if posted {
            if let Ok(mut s) = mmio.write() {
                s.trigger_interrupt(virtio_mmio::INT_VRING);
            }
            self.sync_irq_level(device.info.id);
        }
        posted
    }
}
