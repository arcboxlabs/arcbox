//! Handing guest pages back to the host.
//!
//! Every reclaim path in the device (inflate PFN lists, reported free
//! ranges) ends in [`release`], which validates and aligns the range and
//! hands it to a [`PageReleaser`]. The guest-visible mapping stays intact
//! throughout — the guest may touch a released page again at any time and
//! re-fault it — so "release" means telling the host the contents are
//! garbage and the backing may go.
//!
//! How that is said depends on how the host maps guest RAM:
//!
//! - [`MadviseReleaser`], the default: `madvise(MADV_DONTNEED)`. On Linux
//!   hosts (KVM) the pages are dropped at once and the next guest access
//!   re-faults zero-filled. On Darwin the same call is only a deactivation
//!   hint (measured 2026-07-29 and again 2026-09-25 on macOS 26.4:
//!   `phys_footprint` unchanged), and no `madvise` variant helps once the
//!   guest has dirtied the pages through a stage-2 mapping — the VMM that
//!   owns that mapping must release them itself. `arcbox-vmm`'s HV backend
//!   does so by refreshing the stage-2 mapping, and installs that releaser
//!   with [`VirtioBalloon::with_releaser`](crate::VirtioBalloon::with_releaser).
//!
//! Granularity is the *host* page — 16 KiB on Apple silicon against the
//! guest's 4 KiB — and the releaser only ever sees whole host pages:
//! `madvise` rounds a misaligned range *outward* (XNU's
//! `vm_sanitize_addr_size` truncates the start and rounds the end up) and
//! `hv_vm_unmap` refuses anything finer, so advising one 4 KiB guest page
//! would either discard its three neighbours, which the guest still owns,
//! or fail. Ranges are aligned *inward* here instead, and anything smaller
//! than a host page stays billed rather than risked.

use std::ops::Range;
use std::sync::OnceLock;

use crate::BALLOON_PAGE_SIZE;

/// Returns a host-page-aligned range of guest RAM to the host.
pub trait PageReleaser: Send + Sync {
    /// Releases `len` bytes at host address `host`, which back guest
    /// physical address `gpa`. The caller guarantees `host`, `gpa` and
    /// `len` are host-page-aligned, non-empty, and inside guest RAM, and
    /// that the guest treats the pages as garbage until it takes them back.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the host refused; the pages then stay
    /// billed and the caller logs and moves on.
    fn release(&self, host: *mut u8, gpa: u64, len: usize) -> std::io::Result<()>;
}

/// `madvise(MADV_DONTNEED)`: real reclaim on Linux, a no-op hint on Darwin.
#[derive(Debug, Default, Clone, Copy)]
pub struct MadviseReleaser;

impl PageReleaser for MadviseReleaser {
    fn release(&self, host: *mut u8, _gpa: u64, len: usize) -> std::io::Result<()> {
        // SAFETY: the caller guarantees `host..host+len` is a live,
        // page-aligned part of the guest RAM mapping. `MADV_DONTNEED`
        // never invalidates the mapping, so later guest access stays
        // valid: it re-faults zero-filled where the kernel dropped the
        // page and sees the old contents where it did not.
        let ret = unsafe { libc::madvise(host.cast(), len, libc::MADV_DONTNEED) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// The host's page size, queried once.
pub fn host_page_size() -> usize {
    static HOST_PAGE: OnceLock<usize> = OnceLock::new();
    *HOST_PAGE.get_or_init(|| {
        // SAFETY: `_SC_PAGESIZE` is a valid sysconf key; a non-positive
        // return means "unknown", for which 4 KiB is the conservative floor.
        let r = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(r).ok().filter(|p| *p > 0).unwrap_or(4096)
    })
}

/// The host-page-aligned interior of `[offset, offset + len)`, or `None`
/// when no whole host page fits inside it.
fn host_aligned(offset: usize, len: usize, host_page: usize) -> Option<Range<usize>> {
    let end = offset.checked_add(len)?;
    let start = offset.checked_next_multiple_of(host_page)?;
    let end = end - end % host_page;
    (start < end).then_some(start..end)
}

/// Coalesces a little-endian `u32` PFN array into ascending, disjoint
/// guest-physical byte ranges. A trailing partial `u32` is ignored.
pub fn pfn_runs(buf: &[u8]) -> Vec<Range<u64>> {
    let mut pfns: Vec<u32> = buf
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect();
    pfns.sort_unstable();
    pfns.dedup();
    let mut runs: Vec<Range<u64>> = Vec::new();
    for pfn in pfns {
        let gpa = u64::from(pfn) * BALLOON_PAGE_SIZE;
        match runs.last_mut() {
            Some(run) if run.end == gpa => run.end += BALLOON_PAGE_SIZE,
            _ => runs.push(gpa..gpa + BALLOON_PAGE_SIZE),
        }
    }
    runs
}

/// The guest RAM mapping a release operates on.
#[derive(Debug, Clone, Copy)]
pub struct GuestRam {
    /// Host address of guest physical address `gpa_base`.
    pub base: *mut u8,
    /// Length of the mapping in bytes.
    pub len: usize,
    /// Guest physical address the mapping starts at.
    pub gpa_base: u64,
}

/// Releases the whole host pages inside `[offset, offset + len)` of `ram`
/// through `releaser`. Returns the number of 4 KiB guest pages released.
/// A range that leaves the mapping is logged and released nowhere: it is
/// guest-controlled input.
pub fn release(releaser: &dyn PageReleaser, ram: GuestRam, offset: usize, len: usize) -> u32 {
    let Some(end) = offset.checked_add(len).filter(|end| *end <= ram.len) else {
        tracing::warn!(
            "virtio-balloon: release range {offset:#x}+{len:#x} beyond ram ({:#x})",
            ram.len
        );
        return 0;
    };
    let Some(aligned) = host_aligned(offset, end - offset, host_page_size()) else {
        return 0;
    };
    // SAFETY: `aligned` lies inside `[offset, end) ⊆ [0, ram.len)`, so the
    // pointer stays within the live guest RAM mapping.
    let host = unsafe { ram.base.add(aligned.start) };
    let gpa = ram.gpa_base + aligned.start as u64;
    if let Err(e) = releaser.release(host, gpa, aligned.len()) {
        tracing::warn!(
            "virtio-balloon: releasing gpa {gpa:#x}+{:#x} failed: {e}",
            aligned.len()
        );
        return 0;
    }
    u32::try_from(aligned.len() as u64 / BALLOON_PAGE_SIZE).unwrap_or(u32::MAX)
}
