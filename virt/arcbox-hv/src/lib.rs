//! Safe Rust bindings for Apple Hypervisor.framework (ARM64).
//!
//! Provides VM creation, vCPU management, memory mapping, GICv3 interrupt
//! control, and exit handling for building custom VMMs on macOS.

#[cfg(not(target_os = "macos"))]
compile_error!("arcbox-hv only supports macOS");

#[cfg(not(target_arch = "aarch64"))]
compile_error!("arcbox-hv only supports ARM64");

mod error;
mod exit;
mod ffi;
mod gic;
mod memory;
mod vcpu;
mod vm;

pub use error::{HvError, HvResult};
pub use exit::{ExceptionClass, MmioInfo, VcpuExit};
pub use gic::Gic;
pub use memory::MemoryPermission;
pub use vcpu::HvVcpu;
pub use vm::HvVm;
