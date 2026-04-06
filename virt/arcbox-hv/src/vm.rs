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
    /// Creates a new VM with default configuration (36-bit IPA, ~64 GB).
    ///
    /// Only one VM may exist per process — calling this while a VM is already
    /// alive returns [`HvError::Busy`].
    pub fn new() -> HvResult<Self> {
        // SAFETY: `hv_vm_create(NULL)` creates a VM with default config.
        // The framework enforces single-VM-per-process internally.
        error::check(unsafe { ffi::hv_vm_create(ptr::null_mut()) })?;
        debug!("hypervisor VM created (default IPA size)");
        Ok(Self { _private: () })
    }

    /// Creates a new VM with a custom IPA (Intermediate Physical Address) size.
    ///
    /// The default IPA size is 36 bits (~64 GB). For VMs needing more guest
    /// physical address space (e.g. >32 GB RAM + MMIO), use 40 bits (~1 TB).
    ///
    /// Available since macOS 13.0.
    pub fn with_ipa_size(ipa_bits: u32) -> HvResult<Self> {
        // SAFETY: hv_vm_config_create returns an opaque config object.
        let config = unsafe { ffi::hv_vm_config_create() };
        if config.is_null() {
            return Err(crate::error::HvError::NoResources);
        }
        // SAFETY: config is a valid pointer from hv_vm_config_create.
        error::check(unsafe { ffi::hv_vm_config_set_ipa_size(config, ipa_bits) })?;
        error::check(unsafe { ffi::hv_vm_create(config) })?;
        debug!(ipa_bits, "hypervisor VM created");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires `com.apple.security.hypervisor` entitlement.
    /// Run with: `cargo test -p arcbox-hv -- --ignored`
    #[test]
    #[ignore]
    fn create_and_destroy_vm() {
        let vm = HvVm::new().expect("hv_vm_create failed — is the entitlement set?");
        drop(vm);
    }

    #[test]
    #[ignore]
    fn double_create_returns_busy() {
        let _vm1 = HvVm::new().expect("first VM create failed");
        let result = HvVm::new();
        assert!(
            result.is_err(),
            "second VM create should fail with Busy or Error"
        );
    }

    #[test]
    #[ignore]
    fn map_and_unmap_memory() {
        let vm = HvVm::new().expect("VM create failed");
        let size = 4096;
        let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: Layout is valid and non-zero.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());

        // SAFETY: ptr is a valid 4KB-aligned allocation.
        unsafe {
            vm.map_memory(ptr, 0x4000_0000, size, MemoryPermission::READ_WRITE)
                .expect("map failed");
        }
        vm.unmap_memory(0x4000_0000, size).expect("unmap failed");

        // SAFETY: ptr was allocated with layout.
        unsafe { std::alloc::dealloc(ptr, layout) };
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
