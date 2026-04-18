//! VirtioFS DAX (Direct Access) mapper implementation.
//!
//! Maps host file pages into the guest's DAX window IPA region using
//! Hypervisor.framework's `hv_vm_map` / `hv_vm_unmap` global APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Computes DAX window base IPA — placed immediately after guest RAM.
/// Must be above RAM so `devm_memremap_pages` can register it as ZONE_DEVICE.
pub fn dax_window_base(ram_base: u64, ram_size: u64) -> u64 {
    // Page-align to 2MB boundary (huge page) for optimal TLB usage.
    let after_ram = ram_base + ram_size;
    (after_ram + 0x1F_FFFF) & !0x1F_FFFF
}

/// DAX window size per VirtioFS share (128 MB each).
pub const DAX_WINDOW_PER_SHARE: u64 = 128 * 1024 * 1024;

/// Total DAX window size for a given number of VirtioFS shares.
pub fn dax_window_total(num_shares: usize) -> u64 {
    num_shares.max(1) as u64 * DAX_WINDOW_PER_SHARE
}

/// A single active DAX mapping.
struct DaxMapping {
    host_ptr: *mut u8,
    guest_ipa: u64,
    length: usize,
}

// SAFETY: host_ptr is from mmap, valid for mapping lifetime.
unsafe impl Send for DaxMapping {}

/// Observability counters for DAX activity.
///
/// Updated from the hot path (setup_mapping / remove_mapping) with
/// atomic-relaxed writes — suitable for assertions in integration
/// tests and for lightweight telemetry.
#[derive(Debug, Default, Clone, Copy)]
pub struct DaxStats {
    /// Total successful `setup_mapping` calls since mapper creation.
    pub setup_mappings_count: u64,
    /// Sum of `length` across all successful `setup_mapping` calls (bytes).
    pub setup_mappings_bytes: u64,
    /// Total successful `remove_mapping` calls since mapper creation.
    pub remove_mappings_count: u64,
}

/// DAX mapper backed by Hypervisor.framework global hv_vm_map/unmap.
pub struct HvDaxMapper {
    mappings: Mutex<BTreeMap<u64, DaxMapping>>,
    /// Base IPA of the DAX window (computed from RAM layout).
    base_ipa: u64,
    /// Total DAX window size — mappings must stay within [0, window_size).
    window_size: u64,
    /// Successful setup count (observability).
    setup_count: AtomicU64,
    /// Successful setup bytes (observability).
    setup_bytes: AtomicU64,
    /// Successful remove count (observability).
    remove_count: AtomicU64,
}

impl HvDaxMapper {
    pub fn new(base_ipa: u64, window_size: u64) -> Self {
        Self {
            mappings: Mutex::new(BTreeMap::new()),
            base_ipa,
            window_size,
            setup_count: AtomicU64::new(0),
            setup_bytes: AtomicU64::new(0),
            remove_count: AtomicU64::new(0),
        }
    }

    /// Returns a snapshot of the observability counters. Cheap — reads
    /// three atomic-relaxed loads, allocates nothing.
    pub fn stats(&self) -> DaxStats {
        DaxStats {
            setup_mappings_count: self.setup_count.load(Ordering::Relaxed),
            setup_mappings_bytes: self.setup_bytes.load(Ordering::Relaxed),
            remove_mappings_count: self.remove_count.load(Ordering::Relaxed),
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
        if length == 0 {
            return Err(libc::EINVAL);
        }

        // Bounds check: mapping must fit within the DAX window.
        let end = window_offset.checked_add(length).ok_or(libc::EINVAL)?;
        if end > self.window_size {
            tracing::warn!(
                "DAX setup_mapping out of bounds: offset={:#x} len={:#x} window_size={:#x}",
                window_offset,
                length,
                self.window_size,
            );
            return Err(libc::EINVAL);
        }

        // Hold the lock across overlap check, mmap, hv_vm_map, and insertion
        // to prevent TOCTOU races between concurrent setup_mapping calls.
        // mmap and hv_vm_map are sub-microsecond, so holding the mutex is fine.
        let mut mappings = self.mappings.lock().unwrap();

        // Overlap check: no existing mapping may intersect [window_offset, end).
        for (&existing_offset, existing) in &*mappings {
            let existing_end = existing_offset + existing.length as u64;
            if window_offset < existing_end && end > existing_offset {
                tracing::warn!(
                    "DAX setup_mapping overlap: new [{:#x}..{:#x}) vs existing [{:#x}..{:#x})",
                    window_offset,
                    end,
                    existing_offset,
                    existing_end,
                );
                return Err(libc::EEXIST);
            }
        }

        // Apple Silicon's `hv_vm_map` requires the host virtual address
        // backing to be writable even when the stage-2 mapping is installed
        // read-only. We always open the host mmap with PROT_READ|PROT_WRITE
        // so the kernel accepts the mapping, and then use `MAP_PRIVATE` to
        // ensure writes from the guest (if any) stay local as
        // copy-on-write and never hit the host file. The flags we pass to
        // `hv_vm_map` still reflect the requested guest permissions.
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let mmap_flags = if writable {
            libc::MAP_SHARED
        } else {
            libc::MAP_PRIVATE
        };

        // mmap host file region.
        let host_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length as usize,
                prot,
                mmap_flags,
                host_fd,
                #[allow(clippy::cast_possible_wrap)]
                {
                    file_offset as libc::off_t
                },
            )
        };
        if host_ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO));
        }

        let guest_ipa = self.base_ipa + window_offset;
        // The FUSE SETUPMAPPING protocol only carries READ / WRITE flags,
        // not EXECUTE. But the guest kernel routinely exec's binaries
        // directly out of DAX (that is the whole point of the fast path),
        // so every readable DAX mapping must be installed as stage-2
        // executable too — otherwise the first instruction fetch through
        // the mapping faults with a stage-2 InstructionAbort and the guest
        // process dies before it can run. This matches how qemu-virtiofsd
        // grants `PROT_EXEC` alongside `PROT_READ` on the guest VA.
        let perm = if writable {
            arcbox_hv::MemoryPermission::READ
                | arcbox_hv::MemoryPermission::WRITE
                | arcbox_hv::MemoryPermission::EXEC
        } else {
            arcbox_hv::MemoryPermission::READ | arcbox_hv::MemoryPermission::EXEC
        };

        // Map the host file region into the guest IPA. The DAX window is
        // reserved but not pre-mapped, so `hv_vm_map` installs a fresh
        // stage-2 mapping. If a previous SETUPMAPPING occupied this slot
        // without a matching REMOVEMAPPING, the call will fail — that is a
        // guest-side bug we surface as EEXIST.
        let ret = unsafe {
            arcbox_hv::ffi::hv_vm_map(host_ptr.cast(), guest_ipa, length as usize, perm.bits())
        };
        if ret != 0 {
            tracing::warn!(
                "DAX hv_vm_map failed: ret={:#x} host_ptr={host_ptr:p} ipa={guest_ipa:#x} len={length:#x} perm={:#x}",
                ret as u32,
                perm.bits(),
            );
            unsafe { libc::munmap(host_ptr, length as usize) };
            return Err(libc::EFAULT);
        }

        mappings.insert(
            window_offset,
            DaxMapping {
                host_ptr: host_ptr.cast(),
                guest_ipa,
                length: length as usize,
            },
        );

        self.setup_count.fetch_add(1, Ordering::Relaxed);
        self.setup_bytes.fetch_add(length, Ordering::Relaxed);

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

        // Validate that the requested length matches the stored mapping.
        // Partial unmaps are not supported — return EINVAL and restore.
        if length as usize != mapping.length {
            tracing::warn!(
                "DAX remove_mapping length mismatch: requested={:#x} stored={:#x}",
                length,
                mapping.length,
            );
            mappings.insert(window_offset, mapping);
            return Err(libc::EINVAL);
        }

        // Unmap from guest IPA.
        let ret = unsafe { arcbox_hv::ffi::hv_vm_unmap(mapping.guest_ipa, mapping.length) };
        if ret != 0 {
            tracing::warn!("DAX hv_vm_unmap failed: ret={ret}");
        }

        unsafe { libc::munmap(mapping.host_ptr.cast(), mapping.length) };

        self.remove_count.fetch_add(1, Ordering::Relaxed);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_start_zero() {
        let m = HvDaxMapper::new(0x8000_0000, 128 * 1024 * 1024);
        let s = m.stats();
        assert_eq!(s.setup_mappings_count, 0);
        assert_eq!(s.setup_mappings_bytes, 0);
        assert_eq!(s.remove_mappings_count, 0);
    }

    #[test]
    fn stats_reflect_fetch_add() {
        let m = HvDaxMapper::new(0x8000_0000, 128 * 1024 * 1024);
        // Simulate what the hot path does on a successful setup.
        m.setup_count.fetch_add(3, Ordering::Relaxed);
        m.setup_bytes.fetch_add(4096 * 3, Ordering::Relaxed);
        m.remove_count.fetch_add(1, Ordering::Relaxed);
        let s = m.stats();
        assert_eq!(s.setup_mappings_count, 3);
        assert_eq!(s.setup_mappings_bytes, 4096 * 3);
        assert_eq!(s.remove_mappings_count, 1);
    }
}
