//! VM exit reason parsing for ARM64 Hypervisor.framework.
//!
//! After each `hv_vcpu_run` call, the framework populates an exit info
//! structure. This module converts the raw fields into a typed [`VcpuExit`]
//! enum that VMM code can match on.

use crate::ffi;

// ARM64 Exception Class (EC) values from the syndrome register.
const EC_WFI_WFE: u8 = 0x01;
const EC_HVC: u8 = 0x16;
const EC_SMC: u8 = 0x17;
const EC_SYS_REG: u8 = 0x18;
const EC_DATA_ABORT_LOWER: u8 = 0x24;
const EC_DATA_ABORT_SAME: u8 = 0x25;

/// High-level representation of a vCPU exit.
#[derive(Debug)]
pub enum VcpuExit {
    /// The vCPU run was canceled (e.g. by `hv_vcpus_exit`).
    Canceled,
    /// An exception was taken that the hypervisor cannot handle.
    Exception {
        class: ExceptionClass,
        syndrome: u64,
    },
    /// The virtual timer fired.
    VtimerActivated,
    /// An exit reason the framework returned that we do not recognise.
    Unknown(u32),
}

/// Decoded ARM64 exception class (EC field of ESR_EL2).
#[derive(Debug)]
pub enum ExceptionClass {
    /// Data abort — the guest accessed an unmapped IPA (MMIO).
    DataAbort(MmioInfo),
    /// WFI / WFE — the guest is idle.
    WaitForInterrupt,
    /// HVC #imm16 — hypercall from the guest.
    HypercallHvc(u16),
    /// SMC #imm16 — secure monitor call.
    SmcCall(u16),
    /// MSR/MRS trap — the guest accessed a system register that we trap.
    SystemRegister {
        op0: u8,
        op1: u8,
        crn: u8,
        crm: u8,
        op2: u8,
        /// `true` for MSR (write), `false` for MRS (read).
        is_write: bool,
        /// The general-purpose register (Xt) involved.
        rt: u8,
    },
    /// An EC value we do not explicitly handle.
    Other(u8),
}

/// Describes a guest MMIO access decoded from a data-abort syndrome.
#[derive(Debug)]
pub struct MmioInfo {
    /// Guest physical address that was accessed.
    pub address: u64,
    /// `true` if the guest was writing, `false` if reading.
    pub is_write: bool,
    /// Width of the access: 1, 2, 4, or 8 bytes.
    pub access_size: u8,
    /// The ARM general-purpose register (Xt) used.
    pub register: u8,
    /// For writes: the value the guest wrote. For reads: zero (VMM fills it).
    pub value: u64,
    /// Whether the loaded value should be sign-extended.
    pub sign_extend: bool,
}

/// Parse a raw [`ffi::HvVcpuExitInfo`] into a typed [`VcpuExit`].
pub fn parse_exit(info: &ffi::HvVcpuExitInfo) -> VcpuExit {
    match info.reason {
        ffi::HV_EXIT_REASON_CANCELED => VcpuExit::Canceled,
        ffi::HV_EXIT_REASON_EXCEPTION => {
            let syndrome = info.exception.syndrome;
            let class = parse_exception(syndrome, &info.exception);
            VcpuExit::Exception { class, syndrome }
        }
        ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => VcpuExit::VtimerActivated,
        other => VcpuExit::Unknown(other),
    }
}

/// Decode the exception class from the ESR syndrome value.
fn parse_exception(syndrome: u64, exc: &ffi::HvVcpuExitException) -> ExceptionClass {
    let ec = ((syndrome >> 26) & 0x3f) as u8;

    match ec {
        EC_DATA_ABORT_LOWER | EC_DATA_ABORT_SAME => {
            let is_write = ((syndrome >> 6) & 1) != 0;
            let sas = ((syndrome >> 22) & 3) as u8;
            let access_size = 1u8 << sas;
            let register = ((syndrome >> 16) & 0x1f) as u8;
            let sign_extend = ((syndrome >> 21) & 1) != 0;

            ExceptionClass::DataAbort(MmioInfo {
                address: exc.physical_address,
                is_write,
                access_size,
                register,
                value: 0,
                sign_extend,
            })
        }
        EC_WFI_WFE => ExceptionClass::WaitForInterrupt,
        EC_HVC => {
            let imm16 = (syndrome & 0xffff) as u16;
            ExceptionClass::HypercallHvc(imm16)
        }
        EC_SMC => {
            let imm16 = (syndrome & 0xffff) as u16;
            ExceptionClass::SmcCall(imm16)
        }
        EC_SYS_REG => {
            // ISS encoding for MSR/MRS (ARMv8-A D13.2.37):
            //   [21]    = direction (0 = read/MRS, 1 = write/MSR)
            //   [20:19] = Op0  (bits 20:19 of ISS)
            //   [18:16] = Op2
            //   [15:12] = CRn
            //   [11:8]  = Rt
            //   [7:4]   = CRm
            //   [3:1]   = Op1
            //   [0]     = direction duplicate (same as bit 21 in practice)
            let is_write = (syndrome & 1) == 0; // 0 = write (MSR), 1 = read (MRS)
            let crm = ((syndrome >> 1) & 0xf) as u8;
            let rt = ((syndrome >> 5) & 0x1f) as u8;
            let crn = ((syndrome >> 10) & 0xf) as u8;
            let op1 = ((syndrome >> 14) & 0x7) as u8;
            let op2 = ((syndrome >> 17) & 0x7) as u8;
            let op0 = ((syndrome >> 20) & 0x3) as u8;

            ExceptionClass::SystemRegister {
                op0,
                op1,
                crn,
                crm,
                op2,
                is_write,
                rt,
            }
        }
        other => ExceptionClass::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syndrome value for a data abort with the given parameters.
    fn data_abort_syndrome(ec: u8, sas: u8, is_write: bool, reg: u8, sext: bool) -> u64 {
        let mut s: u64 = 0;
        s |= u64::from(ec) << 26;
        s |= u64::from(sas) << 22;
        if sext {
            s |= 1 << 21;
        }
        s |= u64::from(reg) << 16;
        if is_write {
            s |= 1 << 6;
        }
        s
    }

    #[test]
    fn parse_canceled() {
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_CANCELED,
            exception: ffi::HvVcpuExitException {
                syndrome: 0,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        assert!(matches!(parse_exit(&info), VcpuExit::Canceled));
    }

    #[test]
    fn parse_vtimer() {
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_VTIMER_ACTIVATED,
            exception: ffi::HvVcpuExitException {
                syndrome: 0,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        assert!(matches!(parse_exit(&info), VcpuExit::VtimerActivated));
    }

    #[test]
    fn parse_unknown_reason() {
        let info = ffi::HvVcpuExitInfo {
            reason: 0xff,
            exception: ffi::HvVcpuExitException {
                syndrome: 0,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        assert!(matches!(parse_exit(&info), VcpuExit::Unknown(0xff)));
    }

    #[test]
    fn parse_data_abort_write_4byte() {
        let syndrome = data_abort_syndrome(EC_DATA_ABORT_LOWER, 2, true, 5, false);
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_EXCEPTION,
            exception: ffi::HvVcpuExitException {
                syndrome,
                virtual_address: 0,
                physical_address: 0x0900_0000,
            },
        };
        match parse_exit(&info) {
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(mmio),
                ..
            } => {
                assert!(mmio.is_write);
                assert_eq!(mmio.access_size, 4);
                assert_eq!(mmio.register, 5);
                assert_eq!(mmio.address, 0x0900_0000);
                assert!(!mmio.sign_extend);
            }
            other => panic!("expected DataAbort, got {other:?}"),
        }
    }

    #[test]
    fn parse_data_abort_read_1byte_sign_extend() {
        let syndrome = data_abort_syndrome(EC_DATA_ABORT_SAME, 0, false, 3, true);
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_EXCEPTION,
            exception: ffi::HvVcpuExitException {
                syndrome,
                virtual_address: 0,
                physical_address: 0x4000,
            },
        };
        match parse_exit(&info) {
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(mmio),
                ..
            } => {
                assert!(!mmio.is_write);
                assert_eq!(mmio.access_size, 1);
                assert_eq!(mmio.register, 3);
                assert!(mmio.sign_extend);
            }
            other => panic!("expected DataAbort, got {other:?}"),
        }
    }

    #[test]
    fn parse_wfi() {
        let syndrome = u64::from(EC_WFI_WFE) << 26;
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_EXCEPTION,
            exception: ffi::HvVcpuExitException {
                syndrome,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        match parse_exit(&info) {
            VcpuExit::Exception {
                class: ExceptionClass::WaitForInterrupt,
                ..
            } => {}
            other => panic!("expected WFI, got {other:?}"),
        }
    }

    #[test]
    fn parse_hvc() {
        let syndrome = (u64::from(EC_HVC) << 26) | 0x1234;
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_EXCEPTION,
            exception: ffi::HvVcpuExitException {
                syndrome,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        match parse_exit(&info) {
            VcpuExit::Exception {
                class: ExceptionClass::HypercallHvc(imm),
                ..
            } => {
                assert_eq!(imm, 0x1234);
            }
            other => panic!("expected HVC, got {other:?}"),
        }
    }

    #[test]
    fn parse_smc() {
        let syndrome = (u64::from(EC_SMC) << 26) | 0xabcd;
        let info = ffi::HvVcpuExitInfo {
            reason: ffi::HV_EXIT_REASON_EXCEPTION,
            exception: ffi::HvVcpuExitException {
                syndrome,
                virtual_address: 0,
                physical_address: 0,
            },
        };
        match parse_exit(&info) {
            VcpuExit::Exception {
                class: ExceptionClass::SmcCall(imm),
                ..
            } => {
                assert_eq!(imm, 0xabcd);
            }
            other => panic!("expected SMC, got {other:?}"),
        }
    }

    #[test]
    fn access_size_encoding() {
        // SAS=0 → 1, SAS=1 → 2, SAS=2 → 4, SAS=3 → 8
        for (sas, expected) in [(0u8, 1u8), (1, 2), (2, 4), (3, 8)] {
            let syndrome = data_abort_syndrome(EC_DATA_ABORT_LOWER, sas, false, 0, false);
            let info = ffi::HvVcpuExitInfo {
                reason: ffi::HV_EXIT_REASON_EXCEPTION,
                exception: ffi::HvVcpuExitException {
                    syndrome,
                    virtual_address: 0,
                    physical_address: 0,
                },
            };
            match parse_exit(&info) {
                VcpuExit::Exception {
                    class: ExceptionClass::DataAbort(mmio),
                    ..
                } => {
                    assert_eq!(mmio.access_size, expected, "SAS={sas}");
                }
                other => panic!("expected DataAbort for SAS={sas}, got {other:?}"),
            }
        }
    }
}
