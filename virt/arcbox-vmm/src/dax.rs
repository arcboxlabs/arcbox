//! VirtioFS DAX (Direct Access) mapper implementation.
//!
//! Maps host file pages into the guest's DAX window IPA region using
//! Hypervisor.framework's `hv_vm_map` / `hv_vm_unmap` global APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Computes DAX window base IPA — placed immediately after guest RAM.
/// Must be above RAM so `devm_memremap_pages` can register it as ZONE_DEVICE.
pub fn dax_window_base(ram_base: u64, ram_size: u64) -> u64 {
    // Page-align to 2MB boundary (huge page) for optimal TLB usage.
    let after_ram = ram_base + ram_size;
    (after_ram + 0x1F_FFFF) & !0x1F_FFFF
}

/// Size of the DAX window (256 MB).
pub const DAX_WINDOW_SIZE: u64 = 256 * 1024 * 1024;

/// A single active DAX mapping.
struct DaxMapping {
    host_ptr: *mut u8,
    guest_ipa: u64,
    length: usize,
}

// SAFETY: host_ptr is from mmap, valid for mapping lifetime.
unsafe impl Send for DaxMapping {}

/// DAX mapper backed by Hypervisor.framework global hv_vm_map/unmap.
pub struct HvDaxMapper {
    mappings: Mutex<BTreeMap<u64, DaxMapping>>,
    /// Base IPA of the DAX window (computed from RAM layout).
    base_ipa: u64,
}

impl HvDaxMapper {
    pub fn new(base_ipa: u64) -> Self {
        Self {
            mappings: Mutex::new(BTreeMap::new()),
            base_ipa,
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
        let page_mask = 0xFFF_u64;
        if (file_offset | window_offset | length) & page_mask != 0 {
            return Err(libc::EINVAL);
        }
        if length == 0 || window_offset + length > DAX_WINDOW_SIZE {
            return Err(libc::EINVAL);
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
                #[allow(clippy::cast_possible_wrap)]
                {
                    file_offset as libc::off_t
                },
            )
        };
        if host_ptr == libc::MAP_FAILED {
            return Err(unsafe { *libc::__error() });
        }

        let guest_ipa = self.base_ipa + window_offset;
        let perm = if writable {
            arcbox_hv::MemoryPermission::READ | arcbox_hv::MemoryPermission::WRITE
        } else {
            arcbox_hv::MemoryPermission::READ
        };

        // Map into guest IPA via global HV API.
        let ret = unsafe {
            arcbox_hv::ffi::hv_vm_map(host_ptr.cast(), guest_ipa, length as usize, perm.bits())
        };
        if ret != 0 {
            tracing::warn!("DAX hv_vm_map failed: ret={ret}");
            unsafe { libc::munmap(host_ptr, length as usize) };
            return Err(libc::EFAULT);
        }

        let mut mappings = self.mappings.lock().unwrap();
        mappings.insert(
            window_offset,
            DaxMapping {
                host_ptr: host_ptr.cast(),
                guest_ipa,
                length: length as usize,
            },
        );

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

        // Unmap from guest IPA.
        let ret = unsafe { arcbox_hv::ffi::hv_vm_unmap(mapping.guest_ipa, mapping.length) };
        if ret != 0 {
            tracing::warn!("DAX hv_vm_unmap failed: ret={ret}");
        }

        unsafe { libc::munmap(mapping.host_ptr.cast(), mapping.length) };

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
        let mappings = self.mappings.lock().unwrap();
        for mapping in mappings.values() {
            unsafe {
                arcbox_hv::ffi::hv_vm_unmap(mapping.guest_ipa, mapping.length);
                libc::munmap(mapping.host_ptr.cast(), mapping.length);
            }
        }
    }
}
