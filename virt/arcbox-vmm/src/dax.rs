//! VirtioFS DAX (Direct Access) mapper implementation.
//!
//! Maps host file pages into the guest's DAX window IPA region using
//! Hypervisor.framework's `hv_vm_map` / `hv_vm_unmap` global APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Base IPA of the DAX window in guest physical address space.
/// Placed between MMIO devices (~0x0C00_8000) and RAM (0x4000_0000).
pub const DAX_WINDOW_BASE: u64 = 0x1000_0000;

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
}

impl HvDaxMapper {
    pub fn new() -> Self {
        Self {
            mappings: Mutex::new(BTreeMap::new()),
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

        let guest_ipa = DAX_WINDOW_BASE + window_offset;
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
