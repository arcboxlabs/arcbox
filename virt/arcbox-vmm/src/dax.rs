//! VirtioFS DAX (Direct Access) mapper implementation.
//!
//! Maps host file pages into the guest's DAX window IPA region using
//! Hypervisor.framework's `hv_vm_map` / `hv_vm_unmap` APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;

use arcbox_hv::MemoryPermission;

/// Base IPA of the DAX window in guest physical address space.
/// Placed between MMIO devices (~0x0C00_8000) and RAM (0x4000_0000).
pub const DAX_WINDOW_BASE: u64 = 0x1000_0000;

/// Size of the DAX window (256 MB).
pub const DAX_WINDOW_SIZE: u64 = 256 * 1024 * 1024;

/// A single active DAX mapping.
struct DaxMapping {
    /// Host mmap address.
    host_ptr: *mut u8,
    /// Guest IPA where this is mapped.
    guest_ipa: u64,
    /// Mapping length.
    length: usize,
}

// SAFETY: host_ptr is from mmap, valid for mapping lifetime.
unsafe impl Send for DaxMapping {}

/// DAX mapper backed by Hypervisor.framework.
pub struct HvDaxMapper {
    /// Active mappings keyed by window offset.
    mappings: Mutex<BTreeMap<u64, DaxMapping>>,
    /// HV VM handle for map/unmap.
    vm: std::sync::Arc<arcbox_hv::HvVm>,
}

impl HvDaxMapper {
    pub fn new(vm: std::sync::Arc<arcbox_hv::HvVm>) -> Self {
        Self {
            mappings: Mutex::new(BTreeMap::new()),
            vm,
        }
    }
}

impl arcbox_fs::DaxMapper for HvDaxMapper {
    fn setup_mapping(
        &self,
        host_fd: i32,
        file_offset: u64,
        window_offset: u64,
        length: u64,
        writable: bool,
    ) -> Result<(), i32> {
        // Page-align.
        let page_mask = 0xFFF_u64;
        if (file_offset | window_offset | length) & page_mask != 0 {
            return Err(libc::EINVAL);
        }
        if length == 0 {
            return Err(libc::EINVAL);
        }
        if window_offset + length > DAX_WINDOW_SIZE {
            return Err(libc::ENOMEM);
        }

        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // mmap host file region.
        let host_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length as usize,
                prot,
                libc::MAP_SHARED,
                host_fd,
                file_offset as libc::off_t,
            )
        };
        if host_ptr == libc::MAP_FAILED {
            return Err(unsafe { *libc::__error() });
        }

        let guest_ipa = DAX_WINDOW_BASE + window_offset;
        let perm = if writable {
            MemoryPermission::READ | MemoryPermission::WRITE
        } else {
            MemoryPermission::READ
        };

        // Map into guest IPA.
        if let Err(e) = unsafe {
            self.vm
                .map_memory(host_ptr.cast(), guest_ipa, length as usize, perm)
        } {
            tracing::warn!("DAX hv_vm_map failed: {e}");
            unsafe { libc::munmap(host_ptr, length as usize) };
            return Err(libc::EFAULT);
        }

        let mapping = DaxMapping {
            host_ptr: host_ptr.cast(),
            guest_ipa,
            length: length as usize,
        };

        let mut mappings = self.mappings.lock().unwrap();
        mappings.insert(window_offset, mapping);

        tracing::debug!(
            "DAX setup: window_offset={:#x} len={:#x} ipa={:#x}",
            window_offset,
            length,
            guest_ipa,
        );
        Ok(())
    }

    fn remove_mapping(&self, window_offset: u64, length: u64) -> Result<(), i32> {
        let mut mappings = self.mappings.lock().unwrap();
        let Some(mapping) = mappings.remove(&window_offset) else {
            return Err(libc::ENOENT);
        };

        if mapping.length != length as usize {
            tracing::warn!(
                "DAX remove: length mismatch (expected {}, got {})",
                mapping.length,
                length,
            );
        }

        // Unmap from guest IPA.
        if let Err(e) = self.vm.unmap_memory(mapping.guest_ipa, mapping.length) {
            tracing::warn!("DAX hv_vm_unmap failed: {e}");
        }

        // Release host mapping.
        unsafe {
            libc::munmap(mapping.host_ptr.cast(), mapping.length);
        }

        tracing::debug!(
            "DAX remove: window_offset={:#x} len={:#x}",
            window_offset,
            length,
        );
        Ok(())
    }
}

impl Drop for HvDaxMapper {
    fn drop(&mut self) {
        // Clean up all active mappings.
        let mappings = self.mappings.lock().unwrap();
        for (_, mapping) in mappings.iter() {
            let _ = self.vm.unmap_memory(mapping.guest_ipa, mapping.length);
            unsafe { libc::munmap(mapping.host_ptr.cast(), mapping.length) };
        }
    }
}
