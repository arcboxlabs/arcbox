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

        // Process first item.
        process_item(&ctx, &first);

        // Batch drain: process any immediately available items.
        let mut batch_count = 1u32;
        while batch_count < 32 {
            match rx.try_recv() {
                Ok(item) => {
                    process_item(&ctx, &item);
                    batch_count += 1;
                }
                Err(_) => break,
            }
        }

        let new_used = read_used_idx(&ctx.guest_mem, used_addr);

        // Trigger IRQ if the guest wants notification (EVENT_IDX).
        if should_notify(&ctx.guest_mem, avail_addr, queue_size, old_used, new_used) {
            trigger_irq(&ctx);
        }

        if batch_count > 1 {
            tracing::trace!("blk-io-worker: batch of {} completions", batch_count);
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

fn process_read(ctx: &BlkWorkerContext, item: &BlkWorkItem) -> u8 {
    let mut current_sector = item.sector;
    for &(gpa, len, is_write) in &item.buffers {
        if !is_write {
            continue; // Skip read-only descriptors in a read request.
        }
        // Skip 1-byte status descriptor.
        if len <= 1 {
            continue;
        }
        let Some(buf) = ctx.guest_mem.slice_mut(gpa as usize, len as usize) else {
            tracing::warn!("blk read: GPA {:#x} len {} out of bounds", gpa, len);
            return 1; // IOERR
        };
        #[allow(clippy::cast_possible_wrap)]
        let offset = (current_sector * u64::from(ctx.blk_size)) as libc::off_t;
        let n = unsafe { libc::pread(ctx.raw_fd, buf.as_mut_ptr().cast(), buf.len(), offset) };
        if n < 0 {
            tracing::warn!(
                "blk pread failed at sector {}: {}",
                current_sector,
                std::io::Error::last_os_error()
            );
            return 1; // IOERR
        }
        current_sector += (n as u64) / u64::from(ctx.blk_size);
    }
    0 // OK
}

fn process_write(ctx: &BlkWorkerContext, item: &BlkWorkItem) -> u8 {
    if ctx.read_only {
        return 1; // IOERR
    }
    let mut current_sector = item.sector;
    for &(gpa, len, is_write) in &item.buffers {
        if is_write {
            continue; // Skip write-only descriptors in a write request.
        }
        let Some(buf) = ctx.guest_mem.slice(gpa as usize, len as usize) else {
            tracing::warn!("blk write: GPA {:#x} len {} out of bounds", gpa, len);
            return 1;
        };
        #[allow(clippy::cast_possible_wrap)]
        let offset = (current_sector * u64::from(ctx.blk_size)) as libc::off_t;
        let n = unsafe { libc::pwrite(ctx.raw_fd, buf.as_ptr().cast(), buf.len(), offset) };
        if n < 0 {
            tracing::warn!(
                "blk pwrite failed at sector {}: {}",
                current_sector,
                std::io::Error::last_os_error()
            );
            return 1;
        }
        current_sector += (n as u64) / u64::from(ctx.blk_size);
    }
    0
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

impl FlushBarrier {
    pub fn new() -> Self {
        Self {
            in_flight: AtomicU32::new(0),
        }
    }
}
