//! Returning guest RAM to the host on the HV backend.
//!
//! Guest RAM is an anonymous private mapping in the daemon, exposed to the
//! guest through a Hypervisor.framework stage-2 mapping (`hv_vm_map`).
//! Once the guest has dirtied a page through that stage-2 mapping, no
//! `madvise` from the host side changes what the host is billed for
//! (measured 2026-09-25, macOS 26.4, `stage2_matrix` probe: `MADV_DONTNEED`
//! and `MADV_FREE_REUSABLE` both left `phys_footprint` untouched on
//! guest-written pages, even after `hv_vm_unmap`). The stage-2 pages are
//! pinned by the hypervisor; only the mapping's owner can let go of them.
//!
//! What does work is replacing the backing: unmap the range from stage-2,
//! `mmap(MAP_FIXED)` a fresh anonymous mapping over the same host
//! addresses, and map it into stage-2 again. The old pages are freed with
//! the old mapping (footprint *and* resident size drop at once), the range
//! reads back as zeros, and a later guest write faults a fresh page in and
//! bills it honestly (the probe's `unmap+refresh` row: 513 MB touched → 0
//! after release → 513 MB after the guest rewrote it; `MADV_FREE_REUSABLE`
//! instead re-billed nothing and hid a 768 MB resident set). Measured cost
//! is ~40 µs per 2 MiB range. `hv_vm_unmap` and `hv_vm_map` accept any
//! 16 KiB-aligned sub-range of a live mapping when it is mapped back at
//! the *same* host address, which is all this does; the DAX window's
//! no-overlap trouble (`setup.rs`) is about mapping a *different* host
//! range into a previously mapped IPA.
//!
//! The guest promises not to touch a page while it is being released
//! (balloon §5.5.1; reporting completes the buffer only afterwards), so
//! the window in which the range has no stage-2 mapping is never
//! observed. A guest that breaks that promise takes a stage-2 fault into
//! the vCPU loop's unknown-MMIO path and reads zero — the same thing it
//! would read after a correct release.

use arcbox_hv::MemoryPermission;
use arcbox_virtio::balloon::PageReleaser;

/// Releases guest pages by refreshing their stage-2 and host mappings.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Stage2Refresh;

impl PageReleaser for Stage2Refresh {
    fn release(&self, host: *mut u8, gpa: u64, len: usize) -> std::io::Result<()> {
        // Guest RAM is mapped with every permission (setup.rs); the
        // refreshed range must come back the same way.
        let perms = MemoryPermission::READ_WRITE | MemoryPermission::EXEC;

        // SAFETY: the caller guarantees `gpa..gpa+len` is a live,
        // host-page-aligned part of the guest RAM stage-2 mapping.
        arcbox_hv::check(unsafe { arcbox_hv::ffi::hv_vm_unmap(gpa, len) })
            .map_err(|e| std::io::Error::other(format!("hv_vm_unmap: {e}")))?;

        // Replace the backing. The old pages are freed with the old mapping.
        // SAFETY: `host..host+len` is inside the guest RAM mapping, which
        // outlives the VM; MAP_FIXED over our own anonymous mapping only
        // swaps its backing. Nothing reads the range while it is being
        // replaced: the guest has no stage-2 mapping for it and promised
        // the pages are free, and no device touches guest RAM outside a
        // descriptor the guest handed it.
        let fresh = unsafe {
            libc::mmap(
                host.cast(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        let mmap_result = if fresh == libc::MAP_FAILED {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        };

        // Restore the stage-2 mapping whether or not the refresh worked: a
        // range with no stage-2 mapping is a guest fault waiting to happen.
        // SAFETY: `host` is still a valid mapping of `len` bytes (a failed
        // MAP_FIXED leaves the old mapping in place), and `gpa..gpa+len`
        // was unmapped above.
        let remap =
            arcbox_hv::check(unsafe { arcbox_hv::ffi::hv_vm_map(host, gpa, len, perms.bits()) });
        if let Err(e) = remap {
            // The guest now has a hole in its RAM. Nothing here can mend
            // it; say so as loudly as possible and let the fault surface.
            tracing::error!(
                gpa = format_args!("{gpa:#x}"),
                len = format_args!("{len:#x}"),
                error = %e,
                "hv_vm_map failed to restore a released guest RAM range"
            );
            return Err(std::io::Error::other(format!("hv_vm_map: {e}")));
        }
        mmap_result
    }
}
