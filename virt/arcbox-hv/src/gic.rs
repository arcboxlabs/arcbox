//! Safe wrapper around the macOS 15+ GICv3 emulation APIs.
//!
//! The GIC (Generic Interrupt Controller) is the standard interrupt controller
//! for ARM64. macOS 15 (Sequoia) added `hv_gic_*` APIs to Hypervisor.framework
//! that provide a hardware-accurate GICv3 model, eliminating the need for
//! software emulation.

use std::ptr;

use tracing::debug;

use crate::error::{self, HvResult};
use crate::ffi;

/// Handle to the in-kernel GICv3 emulation.
///
/// Only one GIC instance exists per VM. Creating a [`Gic`] initialises the
/// interrupt controller; dropping it is a no-op (the GIC is destroyed with
/// the VM).
pub struct Gic {
    _private: (),
}

impl Gic {
    /// Creates and initialises the GICv3 with default configuration.
    ///
    /// Must be called after `HvVm::new()` and before any vCPUs are started.
    /// Requires macOS 15+.
    pub fn new() -> HvResult<Self> {
        // SAFETY: `hv_gic_create(NULL)` creates a GIC with default config.
        // A VM must already exist (caller's responsibility).
        error::check(unsafe { ffi::hv_gic_create(ptr::null_mut()) })?;
        debug!("GICv3 created");
        Ok(Self { _private: () })
    }

    /// Resets the GIC to its initial state.
    pub fn reset(&self) -> HvResult<()> {
        // SAFETY: A GIC has been created (we hold `self`).
        error::check(unsafe { ffi::hv_gic_reset() })
    }

    /// Asserts or de-asserts a Shared Peripheral Interrupt (SPI).
    ///
    /// `intid` is the SPI number (32–1019 per the GICv3 spec).
    /// `level` = `true` to assert, `false` to de-assert.
    pub fn set_spi(&self, intid: u32, level: bool) -> HvResult<()> {
        // SAFETY: A GIC has been created. The framework validates `intid`.
        error::check(unsafe { ffi::hv_gic_set_spi(intid, level) })
    }

    /// Returns the base address of the GIC distributor (GICD).
    ///
    /// The VMM must map device memory at this address for the guest to access
    /// the distributor registers.
    pub fn get_distributor_base(&self) -> HvResult<u64> {
        let mut addr: u64 = 0;
        // SAFETY: A GIC has been created, `addr` is a valid out-pointer.
        error::check(unsafe { ffi::hv_gic_get_distributor_base(&raw mut addr) })?;
        Ok(addr)
    }

    /// Returns the base address of the GIC redistributor (GICR) for a given vCPU.
    ///
    /// Each vCPU has its own redistributor. Pass the vCPU ID obtained from
    /// [`HvVcpu::id()`](crate::HvVcpu::id).
    pub fn get_redistributor_base(&self, vcpu_id: u64) -> HvResult<u64> {
        let mut addr: u64 = 0;
        // SAFETY: A GIC has been created and `vcpu_id` is a valid vCPU handle.
        error::check(unsafe { ffi::hv_gic_get_redistributor_base(vcpu_id, &raw mut addr) })?;
        Ok(addr)
    }
}
