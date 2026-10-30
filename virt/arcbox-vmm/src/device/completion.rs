//! The async-worker completion contract, in one type.

use std::sync::{Arc, RwLock};

use super::{DeviceIrqCallback, VirtioMmioState, virtio_mmio};
use crate::irq::Irq;

/// Completes guest I/O for one virtio-mmio device.
///
/// Every async worker that completes guest I/O (blk, net-rx, rx-inject,
/// vsock, console) completes it through this type. [`Self::complete`] sets
/// `interrupt_status`, then asserts the device's SPI, so a worker cannot
/// reorder or drop a step. Asserting the SPI is the whole wake: do not add a
/// vCPU kick after it (see the crate `AGENTS.md`).
#[derive(Clone)]
pub struct IoCompletion {
    mmio_state: Arc<RwLock<VirtioMmioState>>,
    irq: Irq,
    irq_callback: DeviceIrqCallback,
}

impl IoCompletion {
    /// Creates the completion for the device behind `mmio_state` on `irq`.
    #[must_use]
    pub fn new(
        mmio_state: Arc<RwLock<VirtioMmioState>>,
        irq: Irq,
        irq_callback: DeviceIrqCallback,
    ) -> Self {
        Self {
            mmio_state,
            irq,
            irq_callback,
        }
    }

    /// Signals used-ring progress (`INT_VRING`).
    pub fn complete(&self) {
        self.raise(virtio_mmio::INT_VRING);
    }

    /// Sets `reason` in `interrupt_status`, then asserts the SPI.
    pub fn raise(&self, reason: u32) {
        if let Ok(mut state) = self.mmio_state.write() {
            state.trigger_interrupt(reason);
        }
        // A worker thread has no caller to report a failed SPI assert to.
        // The status bit stays set, so the guest sees it on its next interrupt.
        let _ = (self.irq_callback)(self.irq, true);
    }
}
