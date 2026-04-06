//! Safe wrapper around Hypervisor.framework VM lifecycle.
//!
//! Only one VM may exist per process. Creating a second VM will return
//! [`HvError::Busy`].

use std::ptr;

use tracing::debug;

use crate::error::{self, HvResult};
use crate::ffi;
use crate::memory::MemoryPermission;

/// Handle to a Hypervisor.framework VM instance.
///
/// Dropping this struct destroys the VM and releases all associated resources
/// (memory mappings, vCPUs, GIC state, etc.).
pub struct HvVm {
    _private: (),
}

impl HvVm {
    /// Creates a new VM.
    ///
    /// Only one VM may exist per process — calling this while a VM is already
    /// alive returns [`HvError::Busy`].
    pub fn new() -> HvResult<Self> {
        // SAFETY: `hv_vm_create(NULL)` creates a VM with default config.
        // The framework enforces single-VM-per-process internally.
        error::check(unsafe { ffi::hv_vm_create(ptr::null_mut()) })?;
        debug!("hypervisor VM created");
        Ok(Self { _private: () })
    }

    /// Maps a region of host memory into the guest physical address space.
    ///
    /// Both `host_addr` and `ipa` must be page-aligned (4 KiB on ARM64).
    /// `size` must also be a multiple of the page size.
    ///
    /// # Safety
    ///
    /// `host_addr` must point to a valid allocation of at least `size` bytes
    /// that will remain live for as long as the mapping exists.
    pub unsafe fn map_memory(
        &self,
        host_addr: *mut u8,
        ipa: u64,
        size: usize,
        perm: MemoryPermission,
    ) -> HvResult<()> {
        // SAFETY: Caller guarantees `host_addr` validity and alignment.
        // The framework validates IPA range and overlap.
        error::check(unsafe { ffi::hv_vm_map(host_addr, ipa, size, perm.bits()) })
    }

    /// Unmaps a previously mapped guest physical address region.
    ///
    /// `ipa` and `size` must exactly match a prior `map_memory` call.
    pub fn unmap_memory(&self, ipa: u64, size: usize) -> HvResult<()> {
        // SAFETY: The caller ensures the IPA region was previously mapped.
        error::check(unsafe { ffi::hv_vm_unmap(ipa, size) })
    }

    /// Changes the access permissions for a previously mapped IPA region.
    pub fn protect_memory(&self, ipa: u64, size: usize, perm: MemoryPermission) -> HvResult<()> {
        // SAFETY: The caller ensures the IPA region was previously mapped.
        error::check(unsafe { ffi::hv_vm_protect(ipa, size, perm.bits()) })
    }
}

impl Drop for HvVm {
    fn drop(&mut self) {
        // SAFETY: Destroying the VM is always valid when one exists. We own
        // the sole VM handle so no other code can race this call.
        let ret = unsafe { ffi::hv_vm_destroy() };
        if let Err(e) = error::check(ret) {
            tracing::warn!("hv_vm_destroy failed: {e}");
        } else {
            debug!("hypervisor VM destroyed");
        }
    }
}
