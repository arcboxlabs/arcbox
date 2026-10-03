//! IRQ handle for interrupt injection.

use std::sync::Arc;

/// Error type returned by the GIC SPI callback.
pub type IrqError = Box<dyn std::error::Error + Send + Sync>;

/// Thread-safe GIC SPI callback: `(irq_number, level) -> Result<()>`.
pub type IrqCallback = dyn Fn(u32, bool) -> Result<(), IrqError> + Send + Sync;

/// The interrupt delivery mechanism: a thread-safe GIC SPI callback and
/// the line it asserts.
///
/// Asserting the SPI is the whole job — the hypervisor wakes a vCPU
/// sleeping in WFI and injects into a running one on its own.
pub struct IrqHandle {
    /// Fires the GIC SPI for the virtio-net device.
    pub callback: Arc<IrqCallback>,
    /// IRQ number (GIC SPI) for the primary VirtioNet device.
    pub irq: u32,
}

impl IrqHandle {
    /// Asserts the GIC SPI.
    pub fn trigger(&self) {
        let _ = (self.callback)(self.irq, true);
    }
}
