//! Guest RAM backings and the sparse RAM image, shared by the HV checkpoint
//! spikes.

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use super::vcpu::Result;

/// Guest RAM page size for the sparse image.
pub const PAGE: usize = 16 << 10;
/// Offset of the guests' shared data page in RAM.
pub const DATA_OFF: usize = 0x10_0000;

/// A host mapping of guest RAM.
pub struct Ram {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; concurrent access
// from the guest and the host goes through volatile reads or happens while
// the vCPUs are stopped.
unsafe impl Send for Ram {}
// SAFETY: as above.
unsafe impl Sync for Ram {}

impl Ram {
    /// Zeroed anonymous memory.
    pub fn anon(len: usize) -> Result<Self> {
        Self::map(len, libc::MAP_PRIVATE | libc::MAP_ANON, -1)
    }

    /// Anonymous memory filled from `path` (eager restore).
    pub fn read_from(path: &Path, len: usize) -> Result<Self> {
        let ram = Self::anon(len)?;
        File::open(path)?.read_exact(ram.bytes_mut())?;
        Ok(ram)
    }

    /// The checkpoint file mapped copy-on-write (lazy restore). The mapping
    /// keeps the file referenced after it is closed.
    pub fn map_private(path: &Path, len: usize) -> Result<Self> {
        let file = File::open(path)?;
        Self::map(len, libc::MAP_PRIVATE, file.as_raw_fd())
    }

    fn map(len: usize, flags: i32, fd: i32) -> Result<Self> {
        // SAFETY: a fresh private mapping of `len` bytes; checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn bytes_mut(&self) -> &mut [u8] {
        // SAFETY: as `bytes`; callers write only before the guest runs.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Reads a little-endian `T` the guest may be writing concurrently.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "callers pass naturally aligned offsets into a page-aligned mapping"
    )]
    pub fn read<T: Copy>(&self, off: usize) -> T {
        assert!(off + size_of::<T>() <= self.len);
        // SAFETY: in bounds (checked above) and aligned by the caller; the
        // guest writes concurrently, hence the volatile read.
        unsafe { self.ptr.add(off).cast::<T>().read_volatile() }
    }

    /// A `u64` on the shared data page.
    pub fn data(&self, off: usize) -> u64 {
        self.read(DATA_OFF + off)
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        // SAFETY: unmapping the region `map` mapped.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Writes only the non-zero pages, leaving holes for the rest. Returns the
/// page count and the bytes the file occupies.
pub fn write_sparse(path: &Path, bytes: &[u8]) -> Result<(usize, u64)> {
    let file = File::create(path)?;
    file.set_len(bytes.len() as u64)?;
    let mut pages = 0;
    for (i, page) in bytes.chunks(PAGE).enumerate() {
        if page.iter().any(|&b| b != 0) {
            file.write_all_at(page, (i * PAGE) as u64)?;
            pages += 1;
        }
    }
    file.sync_all()?;
    Ok((pages, fs::metadata(path)?.blocks() * 512))
}
