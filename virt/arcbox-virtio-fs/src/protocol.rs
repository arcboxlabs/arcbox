//! FUSE wire-protocol constants — version, init flags, defaults.

/// FUSE kernel protocol major version.
pub const FUSE_KERNEL_VERSION: u32 = 7;

/// FUSE kernel protocol minor version.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 38;

// FUSE init flags
pub const FUSE_ASYNC_READ: u32 = 1 << 0;
pub const FUSE_BIG_WRITES: u32 = 1 << 5;
pub const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;
pub const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;
pub const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;
pub const FUSE_MAP_ALIGNMENT: u32 = 1 << 26;

/// Default max readahead size (128 KB).
pub const DEFAULT_MAX_READAHEAD: u32 = 128 * 1024;

/// Default max write size (128 KB).
pub const DEFAULT_MAX_WRITE: u32 = 128 * 1024;

/// Default max pages per request.
pub const DEFAULT_MAX_PAGES: u16 = 32;

/// Longest request a guest can send.
///
/// That is a `FUSE_WRITE` of [`DEFAULT_MAX_WRITE`], the only `max_write` the
/// session advertises, after its fixed headers (`fuse_in_header` and
/// `fuse_write_in`, 80 bytes): the kernel's `fuse_dev_do_read` requires
/// room for exactly that. One page covers the headers with room to spare,
/// the header room libfuse reserves. A longer descriptor chain is not a
/// FUSE request.
pub const MAX_REQUEST_SIZE: usize = DEFAULT_MAX_WRITE as usize + 4096;
