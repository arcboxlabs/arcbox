//! vCPU and GIC state capture and restore for VM checkpoints.
//!
//! The order a restore must follow, every step measured in the
//! `docs/experiments/2026-10-31-hv-checkpoint-*.md` spikes:
//!
//! 1. Create every vCPU before any vCPU runs (`hv_vcpu_create` blocks while
//!    another vCPU of the VM is inside `hv_vcpu_run`), in the order the
//!    checkpointed VM created them: redistributor frames follow creation
//!    order.
//! 2. [`VcpuState::set_sys`] on every vCPU (MPIDR before the GIC state).
//! 3. [`gic_restore`], once.
//! 4. [`VcpuState::set_icc`], [`VcpuState::set_gprs`],
//!    [`VcpuState::set_simd`], [`VcpuState::set_vtimer`] on every vCPU.
//!
//! On capture, read the GIC with [`gic_capture`] while every vCPU still
//! exists: a blob read after a vCPU is destroyed has lost that vCPU's
//! redistributor, and its PPIs (the timer among them) stay dead after
//! restore.

use serde::{Deserialize, Serialize};

use crate::error::{HvResult, check};
use crate::{HvVcpu, ffi, reg};

/// EL1/EL0 system registers that carry guest state (no ID or debug registers).
///
/// Encodings from `<Hypervisor/hv_vcpu_types.h>`. Restore writes them in
/// this order: MPIDR first, `CNTV_CVAL` before `CNTV_CTL`.
pub const SYS_REGS: &[(&str, u16)] = &[
    ("MPIDR_EL1", 0xc005),
    ("SCTLR_EL1", 0xc080),
    ("ACTLR_EL1", 0xc081),
    ("CPACR_EL1", 0xc082),
    ("TTBR0_EL1", 0xc100),
    ("TTBR1_EL1", 0xc101),
    ("TCR_EL1", 0xc102),
    ("APIAKEYLO_EL1", 0xc108),
    ("APIAKEYHI_EL1", 0xc109),
    ("APIBKEYLO_EL1", 0xc10a),
    ("APIBKEYHI_EL1", 0xc10b),
    ("APDAKEYLO_EL1", 0xc110),
    ("APDAKEYHI_EL1", 0xc111),
    ("APDBKEYLO_EL1", 0xc112),
    ("APDBKEYHI_EL1", 0xc113),
    ("APGAKEYLO_EL1", 0xc118),
    ("APGAKEYHI_EL1", 0xc119),
    ("SPSR_EL1", 0xc200),
    ("ELR_EL1", 0xc201),
    ("SP_EL0", 0xc208),
    ("AFSR0_EL1", 0xc288),
    ("AFSR1_EL1", 0xc289),
    ("ESR_EL1", 0xc290),
    ("FAR_EL1", 0xc300),
    ("PAR_EL1", 0xc3a0),
    ("MAIR_EL1", 0xc510),
    ("AMAIR_EL1", 0xc518),
    ("VBAR_EL1", 0xc600),
    ("CONTEXTIDR_EL1", 0xc681),
    ("TPIDR_EL1", 0xc684),
    ("CNTKCTL_EL1", 0xc708),
    ("CSSELR_EL1", 0xd000),
    ("TPIDR_EL0", 0xde82),
    ("TPIDRRO_EL0", 0xde83),
    ("MDSCR_EL1", 0x8012),
    ("SP_EL1", 0xe208),
    ("CNTP_CVAL_EL0", 0xdf12),
    ("CNTP_CTL_EL0", 0xdf11),
    ("CNTV_CVAL_EL0", 0xdf1a),
    ("CNTV_CTL_EL0", 0xdf19),
];

/// GIC CPU-interface registers, which the GIC blob excludes. `SRE` first:
/// it selects the system-register interface the rest live in. `RPR` is
/// read-only and `SRE_EL2` belongs to a guest with EL2.
pub const ICC_REGS: &[(&str, u16)] = &[
    ("ICC_SRE_EL1", 0xc665),
    ("ICC_CTLR_EL1", 0xc664),
    ("ICC_PMR_EL1", 0xc230),
    ("ICC_BPR0_EL1", 0xc643),
    ("ICC_BPR1_EL1", 0xc663),
    ("ICC_AP0R0_EL1", 0xc644),
    ("ICC_AP1R0_EL1", 0xc648),
    ("ICC_IGRPEN0_EL1", 0xc666),
    ("ICC_IGRPEN1_EL1", 0xc667),
];

/// One vCPU's architectural state.
#[allow(
    clippy::unsafe_derive_deserialize,
    reason = "the unsafe blocks are FFI calls whose safety depends on the calling thread, not on field values; the framework validates the values"
)]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    /// X0–X30, PC, FPCR, FPSR, CPSR (`hv_reg_t` order).
    pub gpr: Vec<u64>,
    /// Q0–Q31.
    pub simd: Vec<[u8; 16]>,
    /// [`SYS_REGS`], in order.
    pub sys: Vec<u64>,
    /// [`ICC_REGS`], in order.
    pub icc: Vec<u64>,
    pub vtimer_masked: bool,
    /// `CNTVOFF`: the guest's virtual counter is host time minus this. A
    /// captured value is absolute; after [`VcpuState::paused_at`] it is
    /// relative to the pause instant, which is what a checkpoint stores.
    pub vtimer_offset: u64,
    /// Host time of the capture, when the guest is known to be stopped.
    pub captured_at: u64,
}

impl VcpuState {
    /// Reads the state of a stopped vCPU. Runs on the vCPU's thread.
    pub fn capture(vcpu: &HvVcpu) -> HvResult<Self> {
        let id = vcpu.id();
        let mut s = Self {
            gpr: (0..=reg::HV_REG_CPSR)
                .map(|r| vcpu.get_reg(r))
                .collect::<HvResult<_>>()?,
            sys: SYS_REGS
                .iter()
                .map(|&(_, r)| vcpu.get_sys_reg(r))
                .collect::<HvResult<_>>()?,
            ..Self::default()
        };
        for q in 0..32 {
            let mut v = [0u8; 16];
            // SAFETY: `id` is live on this thread; `v` is the 16-byte out-buffer.
            check(unsafe { ffi::hv_vcpu_get_simd_fp_reg(id, q, &raw mut v) })?;
            s.simd.push(v);
        }
        for &(_, r) in ICC_REGS {
            let mut v = 0;
            // SAFETY: `id` is live on this thread; `v` is a valid out-pointer.
            check(unsafe { ffi::hv_gic_get_icc_reg(id, r, &raw mut v) })?;
            s.icc.push(v);
        }
        // SAFETY: as above.
        check(unsafe { ffi::hv_vcpu_get_vtimer_offset(id, &raw mut s.vtimer_offset) })?;
        s.captured_at = mach_time();
        // SAFETY: as above.
        check(unsafe { ffi::hv_vcpu_get_vtimer_mask(id, &raw mut s.vtimer_masked) })?;
        Ok(s)
    }

    /// Makes the timer offset relative to `paused_at`, the host time the
    /// guest counts as paused. Use one instant for every vCPU of a VM so
    /// their counters keep their relative offsets.
    #[must_use]
    pub fn paused_at(mut self, paused_at: u64) -> Self {
        self.vtimer_offset = self.vtimer_offset.wrapping_sub(paused_at);
        self
    }

    /// Writes the system registers, MPIDR first.
    pub fn set_sys(&self, vcpu: &HvVcpu) -> HvResult<()> {
        for (&(_, r), &v) in SYS_REGS.iter().zip(&self.sys) {
            vcpu.set_sys_reg(r, v)?;
        }
        Ok(())
    }

    /// Writes the GIC CPU-interface registers. Valid after [`gic_restore`].
    pub fn set_icc(&self, vcpu: &HvVcpu) -> HvResult<()> {
        for (&(_, r), &v) in ICC_REGS.iter().zip(&self.icc) {
            // SAFETY: the vCPU is live on this thread.
            check(unsafe { ffi::hv_gic_set_icc_reg(vcpu.id(), r, v) })?;
        }
        Ok(())
    }

    /// Writes the general-purpose registers, PC and CPSR.
    pub fn set_gprs(&self, vcpu: &HvVcpu) -> HvResult<()> {
        for (r, &v) in (0..=reg::HV_REG_CPSR).zip(&self.gpr) {
            vcpu.set_reg(r, v)?;
        }
        Ok(())
    }

    /// Writes the SIMD/FP registers.
    pub fn set_simd(&self, vcpu: &HvVcpu) -> HvResult<()> {
        for (q, v) in (0..).zip(&self.simd) {
            // SAFETY: the vCPU is live on this thread.
            check(unsafe { ffi::hv_vcpu_set_simd_fp_reg(vcpu.id(), q, v) })?;
        }
        Ok(())
    }

    /// Restores the timer mask and, given the host time the guest resumes
    /// at, the offset of a [`VcpuState::paused_at`] state, so the guest's
    /// counter continues from where it paused. `None` leaves the new
    /// vCPU's default offset.
    pub fn set_vtimer(&self, vcpu: &HvVcpu, resume_at: Option<u64>) -> HvResult<()> {
        if let Some(now) = resume_at {
            resume_vtimer(vcpu, self.vtimer_offset, now)?;
        }
        vcpu.set_vtimer_mask(self.vtimer_masked)
    }

    /// The guest's virtual counter at host time `at`, for a captured state.
    #[must_use]
    pub const fn cntvct(&self, at: u64) -> u64 {
        at.wrapping_sub(self.vtimer_offset)
    }

    /// A named system register, for diagnostics.
    #[must_use]
    pub fn sys_reg(&self, name: &str) -> u64 {
        SYS_REGS
            .iter()
            .position(|&(n, _)| n == name)
            .and_then(|i| self.sys.get(i).copied())
            .unwrap_or(0)
    }
}

/// Continues `vcpu`'s counter from a pause, given a relative offset.
///
/// `relative_offset` is a [`VcpuState::paused_at`] offset and `resume_at`
/// the host time the guest resumes at. Also used for a vCPU that was off
/// at the checkpoint, so it comes up on the same counter as the others.
pub fn resume_vtimer(vcpu: &HvVcpu, relative_offset: u64, resume_at: u64) -> HvResult<()> {
    let offset = resume_at.wrapping_add(relative_offset);
    // SAFETY: the vCPU is live on this thread.
    check(unsafe { ffi::hv_vcpu_set_vtimer_offset(vcpu.id(), offset) })
}

/// Reads Apple's opaque GIC state (distributor and redistributors, not the
/// CPU interfaces). Every vCPU must exist and be outside `hv_vcpu_run`.
pub fn gic_capture() -> HvResult<Vec<u8>> {
    // SAFETY: the caller guarantees the VM is stopped.
    let state = unsafe { ffi::hv_gic_state_create() };
    if state.is_null() {
        return Err(crate::HvError::Error);
    }
    let mut size = 0;
    let mut blob = Vec::new();
    // SAFETY: `state` is a live GIC state object; it is released below on every path.
    let r = check(unsafe { ffi::hv_gic_state_get_size(state, &raw mut size) }).and_then(|()| {
        blob = vec![0; size];
        // SAFETY: `blob` holds exactly the `size` bytes the framework asked for.
        check(unsafe { ffi::hv_gic_state_get_data(state, blob.as_mut_ptr().cast()) })
    });
    // SAFETY: `state` came from `hv_gic_state_create` and is not used again.
    unsafe { ffi::os_release(state) };
    r.map(|()| blob)
}

/// Writes a GIC blob into a new VM: after every vCPU exists and has its
/// MPIDR, before any vCPU runs.
pub fn gic_restore(blob: &[u8]) -> HvResult<()> {
    // SAFETY: the caller guarantees the GIC and all vCPUs exist and none has
    // run; the slice outlives the call.
    check(unsafe { ffi::hv_gic_set_state(blob.as_ptr().cast(), blob.len()) })
}

unsafe extern "C" {
    /// The host timebase the vtimer offset is relative to.
    fn mach_absolute_time() -> u64;
}

/// Host time in the vtimer's timebase.
#[must_use]
pub fn mach_time() -> u64 {
    // SAFETY: no preconditions.
    unsafe { mach_absolute_time() }
}
