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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const HOST_16K: usize = 16 * 1024;

    /// Records every range it is asked to release.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, usize)>>);

    impl PageReleaser for Recorder {
        fn release(&self, _host: *mut u8, gpa: u64, len: usize) -> std::io::Result<()> {
            self.0.lock().unwrap().push((gpa, len));
            Ok(())
        }
    }

    struct Refuser;

    impl PageReleaser for Refuser {
        fn release(&self, _host: *mut u8, _gpa: u64, _len: usize) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EINVAL))
        }
    }

    #[test]
    fn host_aligned_shrinks_inward_and_never_grows() {
        assert_eq!(host_aligned(0x1000, 0x1000, HOST_16K), None);
        assert_eq!(host_aligned(0x1000, 0x8000, HOST_16K), Some(0x4000..0x8000));
        assert_eq!(host_aligned(0x4000, 0x8000, HOST_16K), Some(0x4000..0xC000));
        assert_eq!(host_aligned(0x4000, 0x7FFF, HOST_16K), Some(0x4000..0x8000));
        assert_eq!(host_aligned(0, 0x4000, 4096), Some(0..0x4000));
        assert_eq!(host_aligned(usize::MAX - 0x100, 0x200, HOST_16K), None);
    }

    #[test]
    fn pfn_runs_sort_dedup_and_coalesce() {
        let pfns: Vec<u8> = [7u32, 5, 6, 2, 3, 9, 6]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .chain([0xAA, 0xBB])
            .collect();
        let p = BALLOON_PAGE_SIZE;
        assert_eq!(
            pfn_runs(&pfns),
            vec![2 * p..4 * p, 5 * p..8 * p, 9 * p..10 * p]
        );
        assert!(pfn_runs(&[]).is_empty());
    }

    /// Page-aligned scratch mapping standing in for guest RAM.
    struct Ram {
        ptr: *mut u8,
        len: usize,
    }

    impl Ram {
        fn new(len: usize) -> Self {
            // SAFETY: fresh anonymous private mapping owned by this struct.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(ptr, libc::MAP_FAILED);
            Self {
                ptr: ptr.cast(),
                len,
            }
        }

        fn guest(&self, gpa_base: u64) -> GuestRam {
            GuestRam {
                base: self.ptr,
                len: self.len,
                gpa_base,
            }
        }

        fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: ptr/len describe the live private mapping above.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }
    }

    impl Drop for Ram {
        fn drop(&mut self) {
            // SAFETY: mapping created in new() with this exact len.
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }

    #[test]
    fn release_hands_over_only_whole_host_pages_at_their_guest_address() {
        let host = host_page_size();
        let ram = Ram::new(4 * host);
        let recorder = Recorder::default();
        // One guest page short of a host page on either side: nothing
        // outside `[host, 3*host)` may be released.
        let offset = host - BALLOON_PAGE_SIZE as usize;
        let len = 2 * host + 2 * BALLOON_PAGE_SIZE as usize;
        let released = release(&recorder, ram.guest(0x4000_0000), offset, len);
        assert_eq!(released as usize, 2 * host / BALLOON_PAGE_SIZE as usize);
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![(0x4000_0000 + host as u64, 2 * host)]
        );
    }

    #[test]
    fn release_refuses_ranges_outside_the_mapping_and_reports_refusals() {
        let host = host_page_size();
        let ram = Ram::new(2 * host);
        let recorder = Recorder::default();
        let guest = ram.guest(0);
        assert_eq!(release(&recorder, guest, host, 2 * host), 0);
        assert_eq!(release(&recorder, guest, usize::MAX - host, 2 * host), 0);
        assert_eq!(release(&recorder, guest, 0, BALLOON_PAGE_SIZE as usize), 0);
        assert!(recorder.0.lock().unwrap().is_empty());
        assert_eq!(
            release(&Refuser, guest, 0, host),
            0,
            "a refusal releases nothing"
        );
    }

    #[test]
    fn madvise_releaser_keeps_the_mapping_usable() {
        let host = host_page_size();
        let mut ram = Ram::new(2 * host);
        ram.bytes().fill(0xAB);
        assert_eq!(
            release(&MadviseReleaser, ram.guest(0), 0, host),
            host as u32 / 4096
        );
        // Linux drops the page (reads back zero); Darwin keeps it. Either
        // way the page after it is untouched and the range stays mapped.
        let bytes = ram.bytes();
        assert!(bytes[0] == 0 || bytes[0] == 0xAB);
        assert!(bytes[host..].iter().all(|b| *b == 0xAB));
    }
}
