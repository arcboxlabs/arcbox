//! Safe wrapper around Hypervisor.framework vCPU operations.
//!
//! **Threading constraint**: Apple's Hypervisor.framework requires that all
//! operations on a vCPU happen on the thread that created it. This type is
//! intentionally `!Send` to enforce this at compile time.

use std::marker::PhantomData;
use std::ptr;

use tracing::trace;

use crate::error::{self, HvResult};
use crate::exit::{self, VcpuExit};
use crate::ffi;

/// Handle to a single ARM64 vCPU.
///
/// This type is `!Send` — it can only be used on the thread that created it,
/// as required by Hypervisor.framework.
pub struct HvVcpu {
    /// Opaque framework handle.
    id: u64,
    /// Pointer to the framework-managed exit info structure.
    /// Valid for the lifetime of the vCPU.
    exit_info: *const ffi::HvVcpuExitInfo,
    /// Prevent `Send` — `*const ()` is `!Send`.
    _not_send: PhantomData<*const ()>,
}

impl HvVcpu {
    /// Creates a new vCPU with default configuration.
    ///
    /// Must be called from the thread that will run the vCPU.
    pub fn new() -> HvResult<Self> {
        let mut id: u64 = 0;
        let mut exit: ffi::hv_vcpu_exit_t = ptr::null();

        // SAFETY: We pass valid pointers for the out-parameters. The framework
        // allocates the exit info structure and returns a pointer to it.
        error::check(unsafe { ffi::hv_vcpu_create(&raw mut id, &raw mut exit, ptr::null_mut()) })?;

        trace!(vcpu_id = id, "vCPU created");
        Ok(Self {
            id,
            exit_info: exit,
            _not_send: PhantomData,
        })
    }

    /// Returns the framework vCPU identifier.
    #[inline]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Runs the vCPU until it exits.
    ///
    /// Returns a [`VcpuExit`] describing why execution stopped. The VMM should
    /// handle the exit (e.g. emulate MMIO) and call `run()` again.
    pub fn run(&self) -> HvResult<VcpuExit> {
        // SAFETY: The vCPU was created on this thread and `self.id` is valid.
        error::check(unsafe { ffi::hv_vcpu_run(self.id) })?;

        // SAFETY: `exit_info` was set by `hv_vcpu_create` and remains valid
        // for the lifetime of the vCPU. The framework populates it after each
        // run. We only read from it — no mutation.
        let info = unsafe { &*self.exit_info };
        Ok(exit::parse_exit(info))
    }

    /// Reads a general-purpose register (X0–X30, PC, FPCR, FPSR, CPSR).
    pub fn get_reg(&self, reg: u32) -> HvResult<u64> {
        let mut value: u64 = 0;
        // SAFETY: `self.id` is a valid vCPU, `value` is a valid out-pointer.
        error::check(unsafe { ffi::hv_vcpu_get_reg(self.id, reg, &raw mut value) })?;
        Ok(value)
    }

    /// Writes a general-purpose register.
    pub fn set_reg(&self, reg: u32, value: u64) -> HvResult<()> {
        // SAFETY: `self.id` is a valid vCPU.
        error::check(unsafe { ffi::hv_vcpu_set_reg(self.id, reg, value) })
    }

    /// Reads a system register (e.g. `SCTLR_EL1`, `VBAR_EL1`).
    pub fn get_sys_reg(&self, reg: u16) -> HvResult<u64> {
        let mut value: u64 = 0;
        // SAFETY: `self.id` is a valid vCPU, `value` is a valid out-pointer.
        error::check(unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg, &raw mut value) })?;
        Ok(value)
    }

    /// Writes a system register.
    pub fn set_sys_reg(&self, reg: u16, value: u64) -> HvResult<()> {
        // SAFETY: `self.id` is a valid vCPU.
        error::check(unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg, value) })
    }

    /// Sets or clears pending IRQ and FIQ interrupts on this vCPU.
    ///
    /// When a GIC is not in use, this is the mechanism to inject interrupts.
    pub fn set_pending_interrupt(&self, irq: bool, fiq: bool) -> HvResult<()> {
        // SAFETY: `self.id` is a valid vCPU.
        error::check(unsafe {
            ffi::hv_vcpu_set_pending_interrupt(self.id, ffi::HV_INTERRUPT_TYPE_IRQ, irq)
        })?;
        // SAFETY: Same as above.
        error::check(unsafe {
            ffi::hv_vcpu_set_pending_interrupt(self.id, ffi::HV_INTERRUPT_TYPE_FIQ, fiq)
        })
    }

    /// Masks or unmasks the virtual timer for this vCPU.
    ///
    /// When masked, `VcpuExit::VtimerActivated` will not be reported. This is
    /// useful to suppress exits while the VMM decides how to handle the timer.
    pub fn set_vtimer_mask(&self, masked: bool) -> HvResult<()> {
        // SAFETY: `self.id` is a valid vCPU.
        error::check(unsafe { ffi::hv_vcpu_set_vtimer_mask(self.id, masked) })
    }
}

impl Drop for HvVcpu {
    fn drop(&mut self) {
        // SAFETY: `self.id` is valid and we are on the creation thread (enforced
        // by `!Send`). After this call the handle is no longer usable.
        let ret = unsafe { ffi::hv_vcpu_destroy(self.id) };
        if let Err(e) = error::check(ret) {
            tracing::warn!(vcpu_id = self.id, "hv_vcpu_destroy failed: {e}");
        } else {
            trace!(vcpu_id = self.id, "vCPU destroyed");
        }
    }
}
