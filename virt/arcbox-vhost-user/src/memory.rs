//! Guest RAM shared by the front-end through `SET_MEM_TABLE`.
//!
//! QEMU passes one descriptor per RAM region; with
//! `-object memory-backend-shm,share=on` that is a POSIX shared-memory object
//! (macOS has no memfd). The regions are mapped into one contiguous host
//! reservation that spans the lowest to the highest guest physical address,
//! so a single [`GuestMemWriter`] — the accessor `SplitQueue` is built on —
//! covers all of them. Gaps between regions stay private anonymous memory:
//! a descriptor the guest points into a gap reads zeros and writes scratch
//! pages instead of faulting the host.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use arcbox_virtio_core::GuestMemWriter;

use crate::error::{Error, Result};
use crate::protocol::MemoryRegion;

/// Largest guest-physical span this back-end maps (lowest region start to
/// highest region end). Bounds the reservation a confused front-end can ask
/// for; the System VM's RAM is one region far below it.
const MAX_SPAN: u64 = 1 << 40;

/// Guest RAM mapped from the front-end's region descriptors.
pub struct GuestMemory {
    base: *mut u8,
    span: usize,
    gpa_base: u64,
    regions: Vec<MemoryRegion>,
    writer: Arc<GuestMemWriter>,
}

// SAFETY: `base` is a process-lifetime mapping owned by this value until Drop;
// it is only read through `GuestMemWriter`, which is itself Send + Sync.
unsafe impl Send for GuestMemory {}
// SAFETY: as above; no interior state changes after construction.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Maps `regions`, each from the descriptor at the same position in `fds`.
    pub fn map(regions: &[MemoryRegion], fds: &[OwnedFd]) -> Result<Self> {
        if regions.is_empty() || regions.len() != fds.len() {
            return Err(Error::Protocol(format!(
                "memory table has {} regions but {} descriptors",
                regions.len(),
                fds.len()
            )));
        }
        let page = page_size();
        let mut order: Vec<usize> = (0..regions.len()).collect();
        order.sort_by_key(|&i| regions[i].guest_phys_addr);

        let gpa_base = regions[order[0]].guest_phys_addr;
        let mut end = gpa_base;
        for &i in &order {
            let r = &regions[i];
            let r_end = r
                .guest_phys_addr
                .checked_add(r.memory_size)
                .filter(|_| r.memory_size > 0)
                .ok_or_else(|| Error::Protocol(format!("invalid memory region {r:?}")))?;
            r.userspace_addr
                .checked_add(r.memory_size)
                .ok_or_else(|| Error::Protocol(format!("invalid memory region {r:?}")))?;
            if r.guest_phys_addr < end {
                return Err(Error::Protocol(format!("overlapping memory region {r:?}")));
            }
            if (r.guest_phys_addr - gpa_base) % page != 0 || r.mmap_offset % page != 0 {
                return Err(Error::Unsupported(format!(
                    "memory region {r:?} is not aligned to the {page}-byte host page"
                )));
            }
            end = r_end;
        }
        let span = end - gpa_base;
        if span > MAX_SPAN {
            return Err(Error::Unsupported(format!(
                "guest memory span of {span:#x} bytes exceeds {MAX_SPAN:#x}"
            )));
        }
        let span = span as usize;

        // SAFETY: an anonymous private reservation; no existing mapping is
        // touched.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let base = base.cast::<u8>();
        // From here on Drop unmaps the reservation, including on error.
        let mem = Self {
            base,
            span,
            gpa_base,
            regions: regions.to_vec(),
            // SAFETY: `base` is valid for `span` bytes until `mem` drops, and
            // `mem` keeps the writer's Arc alive only as long as itself.
            writer: Arc::new(unsafe { GuestMemWriter::new(base, span, gpa_base as usize) }),
        };
        for (r, fd) in regions.iter().zip(fds) {
            let offset = (r.guest_phys_addr - gpa_base) as usize;
            let file_offset = libc::off_t::try_from(r.mmap_offset)
                .map_err(|_| Error::Protocol(format!("invalid memory region {r:?}")))?;
            // SAFETY: the target range lies inside our reservation (checked
            // above), so MAP_FIXED replaces only pages we own.
            let mapped = unsafe {
                libc::mmap(
                    base.add(offset).cast(),
                    r.memory_size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fd.as_raw_fd(),
                    file_offset,
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(Error::Io(std::io::Error::other(format!(
                    "mmap of memory region {r:?}: {}",
                    std::io::Error::last_os_error()
                ))));
            }
        }
        Ok(mem)
    }

    /// The accessor `SplitQueue` reads rings and buffers through.
    #[must_use]
    pub fn writer(&self) -> Arc<GuestMemWriter> {
        Arc::clone(&self.writer)
    }

    /// Guest physical address the [`Self::writer`] mapping starts at.
    #[must_use]
    pub const fn gpa_base(&self) -> u64 {
        self.gpa_base
    }

    /// The regions as the front-end described them.
    #[must_use]
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// Translates a front-end virtual address range (how `SET_VRING_ADDR`
    /// names rings) into a guest physical address. The whole range must lie
    /// in one region.
    #[must_use]
    pub fn uva_to_gpa(&self, uva: u64, len: u64) -> Option<u64> {
        self.regions.iter().find_map(|r| {
            let offset = uva.checked_sub(r.userspace_addr)?;
            (offset.checked_add(len)? <= r.memory_size).then(|| r.guest_phys_addr + offset)
        })
    }

    /// Whether the front-end described exactly `regions`. One QEMU process
    /// keeps a RAM block at the same virtual address for its whole life, so
    /// an identical table names the same memory.
    #[must_use]
    pub fn same_layout(&self, regions: &[MemoryRegion]) -> bool {
        self.regions == regions
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // SAFETY: `base..base+span` is the reservation created in `map`; the
        // region mappings inside it go with it.
        unsafe { libc::munmap(self.base.cast(), self.span) };
    }
}

impl std::fmt::Debug for GuestMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestMemory")
            .field("gpa_base", &format_args!("{:#x}", self.gpa_base))
            .field("span", &self.span)
            .field("regions", &self.regions)
            .finish_non_exhaustive()
    }
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    fn backing(len: u64) -> std::fs::File {
        let file = tempfile::tempfile().unwrap();
        file.set_len(len).unwrap();
        file
    }

    #[test]
    fn regions_map_at_their_guest_addresses_and_share_the_backing() {
        let page = page_size();
        let low = backing(2 * page);
        let mut high = backing(4 * page);
        // The high region starts one page into its file and leaves a one-page
        // hole after the low region.
        let regions = [
            MemoryRegion {
                guest_phys_addr: 0x4000_0000 + 3 * page,
                memory_size: 2 * page,
                userspace_addr: 0x9000_0000,
                mmap_offset: page,
            },
            MemoryRegion {
                guest_phys_addr: 0x4000_0000,
                memory_size: 2 * page,
                userspace_addr: 0x7000_0000,
                mmap_offset: 0,
            },
        ];
        let fds = [
            OwnedFd::from(high.try_clone().unwrap()),
            OwnedFd::from(low.try_clone().unwrap()),
        ];
        let mem = GuestMemory::map(&regions, &fds).unwrap();
        let w = mem.writer();

        // A guest write lands in the high file at mmap_offset + 0x10.
        w.write_u32((0x4000_0000 + 3 * page + 0x10) as usize, 0xdead_beef);
        let mut buf = [0u8; 4];
        high.seek(SeekFrom::Start(page + 0x10)).unwrap();
        high.read_exact(&mut buf).unwrap();
        assert_eq!(u32::from_ne_bytes(buf), 0xdead_beef);

        // A host write to the low file is visible to the guest.
        let mut low_w = low.try_clone().unwrap();
        low_w.seek(SeekFrom::Start(8)).unwrap();
        low_w.write_all(&0x1234_5678_u32.to_le_bytes()).unwrap();
        assert_eq!(w.read_u32(0x4000_0008), 0x1234_5678);

        // The hole reads zero and absorbs writes without touching either file.
        let hole = (0x4000_0000 + 2 * page + 8) as usize;
        assert_eq!(w.read_u32(hole), 0);
        w.write_u32(hole, 1);

        // Ring addresses translate per region, never across one.
        assert_eq!(
            mem.uva_to_gpa(0x9000_0010, 16),
            Some(0x4000_0000 + 3 * page + 0x10)
        );
        assert_eq!(mem.uva_to_gpa(0x7000_0000 + 2 * page - 8, 16), None);
        assert_eq!(mem.uva_to_gpa(0x6fff_fff0, 8), None);
        assert!(mem.same_layout(&regions));
    }

    #[test]
    fn overlapping_misaligned_or_unbacked_tables_are_rejected() {
        let page = page_size();
        let file = backing(4 * page);
        let fd = || OwnedFd::from(file.try_clone().unwrap());
        let region = |gpa, size, offset| MemoryRegion {
            guest_phys_addr: gpa,
            memory_size: size,
            userspace_addr: 0x1000_0000_u64.wrapping_add(gpa),
            mmap_offset: offset,
        };
        // Overlap.
        let overlap = [region(0, 2 * page, 0), region(page, page, 0)];
        assert!(GuestMemory::map(&overlap, &[fd(), fd()]).is_err());
        // Misaligned file offset.
        assert!(GuestMemory::map(&[region(0, page, 8)], &[fd()]).is_err());
        // Zero-sized and wrapping regions.
        assert!(GuestMemory::map(&[region(0, 0, 0)], &[fd()]).is_err());
        assert!(GuestMemory::map(&[region(u64::MAX - page + 1, 2 * page, 0)], &[fd()]).is_err());
        // Descriptor count mismatch.
        assert!(GuestMemory::map(&[region(0, page, 0)], &[]).is_err());
        // Span beyond the cap.
        let far = [region(0, page, 0), region(MAX_SPAN, page, 0)];
        assert!(GuestMemory::map(&far, &[fd(), fd()]).is_err());
    }
}
