//! Safe Rust bindings for Apple Hypervisor.framework (ARM64).
//!
//! Provides VM creation, vCPU management, memory mapping, GICv3 interrupt
//! control, and exit handling for building custom VMMs on macOS.
//!
//! On unsupported platforms this crate compiles to an empty library.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod error;
mod exit;
mod ffi;
#[cfg(feature = "gic")]
mod gic;
mod memory;
mod vcpu;
mod vm;

pub use error::{HvError, HvResult};
pub use exit::{ExceptionClass, MmioInfo, VcpuExit};
#[cfg(feature = "gic")]
pub use gic::Gic;
pub use memory::MemoryPermission;
pub use vcpu::HvVcpu;
pub use vm::HvVm;
