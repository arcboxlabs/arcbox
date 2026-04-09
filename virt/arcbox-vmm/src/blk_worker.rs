//! Async block I/O worker for the HV (Hypervisor.framework) backend.
//!
//! Decouples VirtIO block I/O from the vCPU thread. The vCPU parses
//! descriptor chains and submits work items to a channel; a dedicated
//! worker thread performs the actual pread/pwrite and writes completions
//! directly to the guest's used ring, then triggers an IRQ.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crate::device::VirtioMmioState;
use crate::irq::Irq;

/// A single block I/O request parsed from a VirtIO descriptor chain.
pub struct BlkWorkItem {
    /// Descriptor head index — used as the ID in the used ring completion.
    pub head_idx: u16,
    /// Request type.
    pub request_type: BlkRequestType,
    /// Starting sector for read/write.
    pub sector: u64,
    /// Data buffers: (GPA, length, is_write_only).
    /// For reads: write-only buffers to fill with disk data.
    /// For writes: read-only buffers containing data to write to disk.
    pub buffers: Vec<(u64, u32, bool)>,
    /// GPA of the status byte (last byte of last writable descriptor).
    pub status_gpa: u64,
    /// Total byte length across all data descriptors (excluding header/status).
    pub total_data_len: u32,
    /// Used ring GPA (from current QueueConfig, may change after reset).
    pub used_addr: u64,
    /// Avail ring GPA (for EVENT_IDX used_event check).
    pub avail_addr: u64,
    /// Queue size.
    pub queue_size: u16,
}

/// Block request types matching VirtIO spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkRequestType {
    Read,
    Write,
    Flush,
    GetId,
}

// ============================================================================
// Guest Memory Writer
// ============================================================================

/// Safe wrapper around a raw guest memory pointer for the worker thread.
///
/// The pointer is valid for the lifetime of the VM. The worker only writes
/// to device-owned descriptor buffers and the used ring — regions that the
/// VirtIO spec guarantees the guest will not touch until completion.
pub struct GuestMemWriter {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: The pointer originates from a VM-lifetime mmap. The worker
// thread writes only to descriptor buffers (device-owned) and the used
// ring (with Release fences). No concurrent mutation from the guest is
// possible for device-owned buffers per the VirtIO spec.
unsafe impl Send for GuestMemWriter {}
unsafe impl Sync for GuestMemWriter {}

impl GuestMemWriter {
    /// Creates a new writer from the DeviceManager's guest memory.
    ///
    /// # Safety
    /// `ptr` must be valid for `len` bytes for the lifetime of the VM.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn slice_mut(&self, gpa: usize, len: usize) -> Option<&mut [u8]> {
        let end = gpa.checked_add(len)?;
        if end > self.len {
            return None;
        }
        unsafe { Some(std::slice::from_raw_parts_mut(self.ptr.add(gpa), len)) }
    }

    pub fn slice(&self, gpa: usize, len: usize) -> Option<&[u8]> {
        let end = gpa.checked_add(len)?;
        if end > self.len {
            return None;
        }
        unsafe { Some(std::slice::from_raw_parts(self.ptr.add(gpa), len)) }
    }

    fn read_u16(&self, offset: usize) -> u16 {
        if offset + 2 > self.len {
            return 0;
        }
        unsafe {
            let p = self.ptr.add(offset);
            u16::from_le_bytes([*p, *p.add(1)])
        }
    }

    fn write_u16(&self, offset: usize, val: u16) {
        if offset + 2 > self.len {
            return;
        }
        let bytes = val.to_le_bytes();
        unsafe {
            let p = self.ptr.add(offset);
            *p = bytes[0];
            *p.add(1) = bytes[1];
        }
    }

    fn write_u32(&self, offset: usize, val: u32) {
        if offset + 4 > self.len {
            return;
        }
        let bytes = val.to_le_bytes();
        unsafe {
            let p = self.ptr.add(offset);
            *p = bytes[0];
            *p.add(1) = bytes[1];
            *p.add(2) = bytes[2];
            *p.add(3) = bytes[3];
        }
    }

    fn write_byte(&self, offset: usize, val: u8) {
        if offset < self.len {
            unsafe { *self.ptr.add(offset) = val };
        }
    }
}

// ============================================================================
// Worker Context
// ============================================================================

/// Shared context for the block I/O worker thread.
pub struct BlkWorkerContext {
    /// Guest memory (VM-lifetime pointer, offset so index 0 = GPA 0).
    pub guest_mem: GuestMemWriter,
    /// Raw fd for pread/pwrite (owned by VirtioBlock's File handle).
    pub raw_fd: i32,
    /// Block size (typically 512).
    pub blk_size: u32,
    /// Whether the device is read-only.
    pub read_only: bool,
    /// Device ID string for GetId requests.
    pub device_id: String,
    /// MMIO state for setting interrupt_status.
    pub mmio_state: Arc<RwLock<VirtioMmioState>>,
    /// IRQ trigger callback.
    pub irq_callback: Arc<dyn Fn(Irq, bool) -> crate::error::Result<()> + Send + Sync>,
    /// IRQ number for this block device.
    pub irq: Irq,
    /// VM shutdown flag.
    pub running: Arc<AtomicBool>,
    /// Shared flush barrier for multi-queue flush synchronization.
    pub flush_barrier: Arc<FlushBarrier>,
}

// SAFETY: All fields are either Send+Sync or raw pointers wrapped in
// GuestMemWriter which is Send+Sync.
unsafe impl Send for BlkWorkerContext {}

// ============================================================================
// Worker Loop
// ============================================================================

/// Main loop for the block I/O worker thread.
///
/// Receives work items from the vCPU thread, performs pread/pwrite,
/// writes completions to the used ring, and triggers IRQs.
pub fn blk_io_worker_loop(ctx: BlkWorkerContext, rx: std::sync::mpsc::Receiver<BlkWorkItem>) {
    tracing::info!(
        "blk-io-worker started (fd={}, blk_size={})",
        ctx.raw_fd,
        ctx.blk_size
    );

    while let Ok(first) = rx.recv() {
        if !ctx.running.load(Ordering::Relaxed) {
            break;
        }

        let used_addr = first.used_addr;
        let avail_addr = first.avail_addr;
        let queue_size = first.queue_size;
        let old_used = read_used_idx(&ctx.guest_mem, used_addr);

        // Collect batch: first item + up to 31 more from try_recv.
        let mut batch = Vec::with_capacity(32);
        batch.push(first);
        while batch.len() < 32 {
            match rx.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }

        // Sort by (request_type, sector) to enable merging.
        // Flush/GetId items are processed first (low ordinal).
        if batch.len() > 1 {
            batch.sort_unstable_by(|a, b| {
                let type_ord = |t: &BlkRequestType| match t {
                    BlkRequestType::Flush => 0u8,
                    BlkRequestType::GetId => 1,
                    BlkRequestType::Read => 2,
                    BlkRequestType::Write => 3,
                };
                type_ord(&a.request_type)
                    .cmp(&type_ord(&b.request_type))
                    .then(a.sector.cmp(&b.sector))
            });
        }

        // Process sorted batch. Adjacent same-type items with contiguous
        // sectors are merged into a single preadv/pwritev call.
        let mut i = 0;
        while i < batch.len() {
            let item = &batch[i];

            // Non-mergeable types: process individually.
            if !matches!(
                item.request_type,
                BlkRequestType::Read | BlkRequestType::Write
            ) {
                process_item(&ctx, item);
                i += 1;
                continue;
            }

            // Find merge run: consecutive items with same type and
            // contiguous sectors.
            let mut end = i + 1;
            while end < batch.len()
                && batch[end].request_type == item.request_type
                && batch[end].sector
                    == batch[end - 1].sector
                        + u64::from(batch[end - 1].total_data_len) / u64::from(ctx.blk_size)
            {
                end += 1;
            }

            if end == i + 1 {
                // Single item, no merge possible.
                process_item(&ctx, item);
            } else {
                // Merged run: build combined iovec from all items.
                process_merged(&ctx, &batch[i..end]);
            }
            i = end;
        }

        let new_used = read_used_idx(&ctx.guest_mem, used_addr);

        if should_notify(&ctx.guest_mem, avail_addr, queue_size, old_used, new_used) {
            trigger_irq(&ctx);
        }

        if batch.len() > 1 {
            tracing::trace!("blk-io-worker: batch of {} items", batch.len());
        }
    }

    tracing::info!("blk-io-worker exiting");
}

/// Processes a single block I/O work item.
fn process_item(ctx: &BlkWorkerContext, item: &BlkWorkItem) {
    let is_io = matches!(
        item.request_type,
        BlkRequestType::Read | BlkRequestType::Write
    );

    if is_io {
        ctx.flush_barrier.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    let status = match item.request_type {
        BlkRequestType::Read => process_read(ctx, item),
        BlkRequestType::Write => process_write(ctx, item),
        BlkRequestType::Flush => {
            // Wait for all in-flight I/O across all queues to complete.
            // This is a spin-wait; in practice flush is rare and in-flight
            // drains quickly once no new I/O is submitted.
            while ctx.flush_barrier.in_flight.load(Ordering::Acquire) > 0 {
                std::hint::spin_loop();
            }
            process_flush(ctx)
        }
        BlkRequestType::GetId => process_get_id(ctx, item),
    };

    if is_io {
        ctx.flush_barrier.in_flight.fetch_sub(1, Ordering::Release);
    }

    // Write status byte.
    ctx.guest_mem.write_byte(item.status_gpa as usize, status);

    // Compute total bytes for the used ring entry.
    let total_bytes = if status == 0 {
        item.total_data_len + 1 // data + status byte
    } else {
        1 // just status byte
    };

    // Write used ring entry.
    write_used_entry(
        &ctx.guest_mem,
        item.used_addr,
        item.queue_size,
        item.head_idx,
        total_bytes,
    );
}

/// Reads using preadv — single syscall for scatter-gather buffers.
fn process_read(ctx: &BlkWorkerContext, item: &BlkWorkItem) -> u8 {
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    for &(gpa, len, is_write) in &item.buffers {
        if !is_write || len <= 1 {
            continue;
        }
        let Some(buf) = ctx.guest_mem.slice_mut(gpa as usize, len as usize) else {
            tracing::warn!("blk read: GPA {:#x} len {} out of bounds", gpa, len);
            return 1;
        };
        iovecs.push(libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        });
    }
    if iovecs.is_empty() {
        return 0;
    }
    #[allow(clippy::cast_possible_wrap)]
    let offset = (item.sector * u64::from(ctx.blk_size)) as libc::off_t;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n = unsafe { libc::preadv(ctx.raw_fd, iovecs.as_ptr(), iovecs.len() as i32, offset) };
    if n < 0 {
        tracing::warn!(
            "blk preadv failed at sector {}: {}",
            item.sector,
            std::io::Error::last_os_error()
        );
        return 1;
    }
    0
}

/// Writes using pwritev — single syscall for scatter-gather buffers.
fn process_write(ctx: &BlkWorkerContext, item: &BlkWorkItem) -> u8 {
    if ctx.read_only {
        return 1;
    }
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    for &(gpa, len, is_write) in &item.buffers {
        if is_write {
            continue;
        }
        let Some(buf) = ctx.guest_mem.slice(gpa as usize, len as usize) else {
            tracing::warn!("blk write: GPA {:#x} len {} out of bounds", gpa, len);
            return 1;
        };
        iovecs.push(libc::iovec {
            iov_base: buf.as_ptr().cast_mut().cast(),
            iov_len: buf.len(),
        });
    }
    if iovecs.is_empty() {
        return 0;
    }
    #[allow(clippy::cast_possible_wrap)]
    let offset = (item.sector * u64::from(ctx.blk_size)) as libc::off_t;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n = unsafe { libc::pwritev(ctx.raw_fd, iovecs.as_ptr(), iovecs.len() as i32, offset) };
    if n < 0 {
        tracing::warn!(
            "blk pwritev failed at sector {}: {}",
            item.sector,
            std::io::Error::last_os_error()
        );
        return 1;
    }
    0
}

/// Processes a merged run of same-type items with contiguous sectors
/// using a single preadv/pwritev syscall. Each item still gets its own
/// used ring completion.
fn process_merged(ctx: &BlkWorkerContext, items: &[BlkWorkItem]) {
    let is_read = items[0].request_type == BlkRequestType::Read;
    let start_sector = items[0].sector;

    // Track in-flight for all items in the merge group.
    ctx.flush_barrier
        .in_flight
        .fetch_add(items.len() as u32, Ordering::Relaxed);

    // Build combined iovec from all items.
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    for item in items {
        for &(gpa, len, is_write_flag) in &item.buffers {
            // For reads: use write-only descriptors. For writes: use read-only.
            let want = if is_read {
                is_write_flag
            } else {
                !is_write_flag
            };
            if !want || len <= 1 {
                continue;
            }
            if is_read {
                if let Some(buf) = ctx.guest_mem.slice_mut(gpa as usize, len as usize) {
                    iovecs.push(libc::iovec {
                        iov_base: buf.as_mut_ptr().cast(),
                        iov_len: buf.len(),
                    });
                }
            } else if let Some(buf) = ctx.guest_mem.slice(gpa as usize, len as usize) {
                iovecs.push(libc::iovec {
                    iov_base: buf.as_ptr().cast_mut().cast(),
                    iov_len: buf.len(),
                });
            }
        }
    }

    // Single merged I/O syscall.
    #[allow(clippy::cast_possible_wrap)]
    let offset = (start_sector * u64::from(ctx.blk_size)) as libc::off_t;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n = if is_read {
        unsafe { libc::preadv(ctx.raw_fd, iovecs.as_ptr(), iovecs.len() as i32, offset) }
    } else {
        unsafe { libc::pwritev(ctx.raw_fd, iovecs.as_ptr(), iovecs.len() as i32, offset) }
    };

    let status: u8 = if n < 0 {
        tracing::warn!(
            "blk merged {} failed at sector {}: {}",
            if is_read { "preadv" } else { "pwritev" },
            start_sector,
            std::io::Error::last_os_error()
        );
        1
    } else {
        0
    };

    // Write individual used ring entries for each item.
    for item in items {
        ctx.guest_mem.write_byte(item.status_gpa as usize, status);
        let total_bytes = if status == 0 {
            item.total_data_len + 1
        } else {
            1
        };
        write_used_entry(
            &ctx.guest_mem,
            item.used_addr,
            item.queue_size,
            item.head_idx,
            total_bytes,
        );
    }

    ctx.flush_barrier
        .in_flight
        .fetch_sub(items.len() as u32, Ordering::Release);

    tracing::trace!(
        "blk merged {} items: sector {}..+{}, {} iovecs, {} bytes",
        items.len(),
        start_sector,
        items.last().map_or(0, |i| i.sector
            + u64::from(i.total_data_len) / u64::from(ctx.blk_size))
            - start_sector,
        iovecs.len(),
        n,
    );
}

fn process_flush(ctx: &BlkWorkerContext) -> u8 {
    let ret = unsafe { libc::fsync(ctx.raw_fd) };
    if ret < 0 {
        tracing::warn!("blk fsync failed: {}", std::io::Error::last_os_error());
        1
    } else {
        0
    }
}

fn process_get_id(ctx: &BlkWorkerContext, item: &BlkWorkItem) -> u8 {
    let id_bytes = ctx.device_id.as_bytes();
    for &(gpa, len, is_write) in &item.buffers {
        if !is_write || len <= 1 {
            continue;
        }
        if let Some(buf) = ctx.guest_mem.slice_mut(gpa as usize, len as usize) {
            let copy_len = id_bytes.len().min(buf.len());
            buf[..copy_len].copy_from_slice(&id_bytes[..copy_len]);
        }
        break;
    }
    0
}

// ============================================================================
// Used Ring Operations
// ============================================================================

fn read_used_idx(guest_mem: &GuestMemWriter, used_addr: u64) -> u16 {
    guest_mem.read_u16(used_addr as usize + 2)
}

fn write_used_entry(
    guest_mem: &GuestMemWriter,
    used_addr: u64,
    queue_size: u16,
    head_idx: u16,
    total_bytes: u32,
) {
    let used_idx = read_used_idx(guest_mem, used_addr);
    let entry_off = used_addr as usize + 4 + ((used_idx as usize) % (queue_size as usize)) * 8;
    guest_mem.write_u32(entry_off, head_idx as u32);
    guest_mem.write_u32(entry_off + 4, total_bytes);
    std::sync::atomic::fence(Ordering::Release);
    guest_mem.write_u16(used_addr as usize + 2, used_idx.wrapping_add(1));
}

// ============================================================================
// IRQ
// ============================================================================

/// Checks whether the guest wants an interrupt (EVENT_IDX suppression).
fn should_notify(
    guest_mem: &GuestMemWriter,
    avail_addr: u64,
    queue_size: u16,
    old_used: u16,
    new_used: u16,
) -> bool {
    if old_used == new_used {
        return false;
    }
    let used_event_off = avail_addr as usize + 4 + 2 * (queue_size as usize);
    let used_event = guest_mem.read_u16(used_event_off);
    new_used.wrapping_sub(used_event).wrapping_sub(1) < new_used.wrapping_sub(old_used)
}

fn trigger_irq(ctx: &BlkWorkerContext) {
    // Set interrupt_status on MMIO state.
    if let Ok(mut s) = ctx.mmio_state.write() {
        s.trigger_interrupt(1); // INT_VRING
    }
    // Fire GIC SPI.
    let _ = (ctx.irq_callback)(ctx.irq, true);
}

// ============================================================================
// Per-device async state (stored on DeviceManager)
// ============================================================================

/// Per-queue I/O worker handle.
pub struct BlkQueueWorker {
    pub tx: std::sync::mpsc::Sender<BlkWorkItem>,
    pub last_avail_idx: AtomicU16,
}

/// Per-block-device async I/O state. Holds one worker per queue.
pub struct BlkWorkerHandle {
    /// Workers indexed by queue_idx.
    pub queues: Vec<BlkQueueWorker>,
}

impl BlkWorkerHandle {
    /// Returns the worker for a given queue index, or None.
    pub fn get_queue(&self, queue_idx: u16) -> Option<&BlkQueueWorker> {
        self.queues.get(queue_idx as usize)
    }
}

/// Shared flush barrier across all queues of a device.
/// Used to implement VIRTIO_BLK_T_FLUSH correctly with multi-queue:
/// flush must wait for all in-flight I/O across all queues to complete.
pub struct FlushBarrier {
    pub in_flight: AtomicU32,
}

impl Default for FlushBarrier {
    fn default() -> Self {
        Self {
            in_flight: AtomicU32::new(0),
        }
    }
}

impl FlushBarrier {
    pub fn new() -> Self {
        Self::default()
    }
}
