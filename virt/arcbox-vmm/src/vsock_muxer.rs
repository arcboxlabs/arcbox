//! Unified vsock muxer thread: TX processing (guest->host) + RX injection
//! (host->guest) in a single kqueue event loop.
//!
//! Replaces the split architecture where the vCPU thread processed TX via
//! MMIO handler (device.rs) and `vsock_rx_worker.rs` handled RX injection.
//! The muxer merges both paths into one thread that owns the
//! `VsockConnectionManager` (via `Arc<Mutex<>>` for Phase 2 compatibility).
//!
//! The vCPU signals the muxer via a pipe when the guest kicks the TX queue.
//! The muxer also polls host socketpair fds for incoming RX data and
//! injects packets into the guest RX virtqueue with coalesced IRQs.
//!
//! Event loop structure:
//! ```text
//! loop {
//!     1. Accept allocation requests (non-blocking channel drain)
//!     2. Accept inline connections (non-blocking channel drain)
//!     3. Dynamic fd registration (kqueue add/remove)
//!     4. Drain control packets from backend_rxq (REQUEST, CREDIT_UPDATE, etc.)
//!     5. kqueue wait (socketpair fds + tx_kick_pipe_fd, 1ms timeout)
//!     6. If tx_kick_pipe readable -> process_tx_queue()
//!     7. Process readable socketpair fds -> RX data injection
//!     8. End-of-cycle: flush coalesced RX IRQ
//! }
//! ```

use std::io::Read;
use std::os::unix::io::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arcbox_virtio::vsock::{VsockAddr, VsockHeader, VsockHostConnections, VsockOp};

use crate::blk_worker::GuestMemWriter;
use crate::device::VirtioMmioState;
use crate::irq::Irq;
use crate::vsock_manager::{RxOps, TX_BUFFER_SIZE, VsockConnectionId, VsockConnectionManager};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum packets to inject per kqueue wakeup before flushing IRQ.
const BATCH_SIZE: usize = 64;

/// Maximum bytes to read from a single socket per iteration.
const MAX_READ_SIZE: usize = 65536;

/// Interrupt coalescing timeout (latency bound). If this many microseconds
/// elapse since the first unflushed packet, fire IRQ unconditionally.
const COALESCE_TIMEOUT: Duration = Duration::from_micros(50);

/// kqueue poll timeout -- bounds the shutdown, new-fd check, and TX kick
/// response frequency. Kept short so the muxer promptly detects new
/// connections and allocation requests.
const POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// Backoff when RX descriptors are exhausted. Prevents busy-spinning
/// while the guest processes outstanding descriptors.
const DESCRIPTOR_BACKOFF: Duration = Duration::from_micros(50);

// ---------------------------------------------------------------------------
// TX Queue Configuration
// ---------------------------------------------------------------------------

/// TX virtqueue layout, captured at DRIVER_OK time.
/// Same pattern as `VsockRxQueueConfig` -- the GPAs are stable for the
/// lifetime of the device (until RESET).
pub struct VsockTxQueueConfig {
    pub desc_gpa: u64,
    pub avail_gpa: u64,
    pub used_gpa: u64,
    pub size: u16,
}

// ---------------------------------------------------------------------------
// RX Queue Configuration (re-exported from rx_worker for convenience)
// ---------------------------------------------------------------------------

/// RX queue layout, captured at DRIVER_OK time.
pub struct VsockRxQueueConfig {
    pub desc_gpa: u64,
    pub avail_gpa: u64,
    pub used_gpa: u64,
    pub size: u16,
}

// ---------------------------------------------------------------------------
// Allocation Request
// ---------------------------------------------------------------------------

/// A connection allocation request sent from the daemon to the muxer.
/// The daemon cannot call `VsockConnectionManager::allocate` directly
/// because the muxer owns the lock. Instead, requests arrive on a channel
/// and the muxer processes them at the top of each loop iteration.
pub struct VsockAllocRequest {
    pub guest_port: u32,
    pub guest_cid: u64,
    pub internal_fd: OwnedFd,
    /// The muxer sends the allocated ID and injected-notify receiver back
    /// through this channel so the daemon can wait on it.
    pub reply_tx: std::sync::mpsc::Sender<(VsockConnectionId, std::sync::mpsc::Receiver<()>)>,
}

// ---------------------------------------------------------------------------
// Inline Connection (promoted port-forwarding path)
// ---------------------------------------------------------------------------

/// A port-forwarding TCP connection promoted to the inline inject path.
///
/// Instead of relaying through a socketpair, the worker reads directly
/// from this TCP stream into guest descriptor buffers, prepending a
/// 44-byte VsockHeader inline. Eliminates 4 copies and 2 syscalls
/// compared to the socketpair path.
pub struct VsockInlineConn {
    /// Host-side TCP stream (non-blocking, clone of the port forwarder's stream).
    pub stream: std::net::TcpStream,
    /// Vsock connection identity.
    pub conn_id: VsockConnectionId,
    /// Guest CID (typically 3).
    pub guest_cid: u64,
    /// Host stream reached EOF.
    pub host_eof: bool,
}

// SAFETY: TcpStream is Send; all other fields are plain data.
unsafe impl Send for VsockInlineConn {}

// ---------------------------------------------------------------------------
// Muxer Context
// ---------------------------------------------------------------------------

/// Shared context for the vsock muxer thread. Passed to `vsock_muxer_loop`
/// at spawn time.
pub struct VsockMuxerContext {
    pub guest_mem: GuestMemWriter,
    pub rx_queue: VsockRxQueueConfig,
    pub tx_queue: VsockTxQueueConfig,
    pub mmio_state: Arc<RwLock<VirtioMmioState>>,
    pub irq_callback: Arc<dyn Fn(Irq, bool) -> crate::error::Result<()> + Send + Sync>,
    pub irq: Irq,
    pub exit_vcpus: Arc<dyn Fn() + Send + Sync>,

    /// Connection manager. The muxer is the SOLE user, but we keep
    /// `Arc<Mutex<>>` for Phase 2 compatibility with existing callers
    /// that hold a reference (daemon allocation path).
    pub vsock_mgr: Arc<Mutex<VsockConnectionManager>>,

    pub running: Arc<AtomicBool>,

    /// Pipe read end -- the vCPU writes a byte when the guest kicks the TX
    /// queue. Registered in kqueue as EVFILT_READ. When readable, drain
    /// all bytes (coalesce kicks) and call `process_tx_queue`.
    pub tx_kick_fd: i32,

    /// Channel for receiving allocation requests from the daemon.
    pub alloc_rx: crossbeam_channel::Receiver<VsockAllocRequest>,

    /// Channel for receiving promoted port-forward connections.
    pub inline_conn_rx: crossbeam_channel::Receiver<VsockInlineConn>,
}

// SAFETY: All fields are Send+Sync or wrapped in Arc<Mutex/RwLock>.
// tx_kick_fd is a plain file descriptor (pipe read end).
unsafe impl Send for VsockMuxerContext {}

// ---------------------------------------------------------------------------
// kqueue token constants
// ---------------------------------------------------------------------------

/// Sentinel token for the TX kick pipe in kqueue. Chosen to never collide
/// with a real socketpair fd (which are positive). We use usize::MAX.
const TX_KICK_TOKEN: usize = usize::MAX;

// ---------------------------------------------------------------------------
// IRQ helpers (identical to vsock_rx_worker)
// ---------------------------------------------------------------------------

/// Triggers a vsock interrupt and kicks vCPUs out of WFI.
/// hv_vcpus_exit is REQUIRED -- without it, vCPUs stay in WFI and never
/// see the pending GIC SPI (causes RCU stall from WFI deadlock).
fn trigger_irq(ctx: &VsockMuxerContext) {
    if let Ok(mut s) = ctx.mmio_state.write() {
        s.trigger_interrupt(1); // INT_VRING
    }
    let _ = (ctx.irq_callback)(ctx.irq, true);
    (ctx.exit_vcpus)();
}

/// Conditionally triggers an interrupt based on EVENT_IDX suppression.
/// Only fires GIC SPI + hv_vcpus_exit when the guest actually wants a
/// notification (used_event crossed). Prevents interrupt storms that
/// starve guest userspace and cause RCU stalls.
fn maybe_notify(ctx: &VsockMuxerContext, old_used: u16, new_used: u16) {
    if crate::virtqueue_util::should_notify(
        &ctx.guest_mem,
        ctx.rx_queue.avail_gpa,
        ctx.rx_queue.size,
        old_used,
        new_used,
    ) {
        trigger_irq(ctx);
    }
}

/// Writes avail_event into the RX used ring for EVENT_IDX.
fn write_avail_event_rx(ctx: &VsockMuxerContext, _used_idx: u16) {
    let q_size = ctx.rx_queue.size as usize;
    let avail_event_off = ctx.rx_queue.used_gpa as usize + 4 + q_size * 8;
    let avail_idx = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
    ctx.guest_mem.write_u16(avail_event_off, avail_idx);
}

// ---------------------------------------------------------------------------
// RX descriptor helpers (identical to vsock_rx_worker)
// ---------------------------------------------------------------------------

/// Peeks the next available RX descriptor chain's total writable capacity
/// (bytes). Returns 0 if no descriptors are available.
fn peek_rx_capacity(ctx: &VsockMuxerContext, used_idx: u16) -> usize {
    let q_size = ctx.rx_queue.size as usize;
    if q_size == 0 {
        return 0;
    }

    std::sync::atomic::fence(Ordering::Acquire);
    let avail_idx = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
    if used_idx == avail_idx {
        return 0;
    }

    let ring_off = ctx.rx_queue.avail_gpa as usize + 4 + 2 * ((used_idx as usize) % q_size);
    let head_idx = ctx.guest_mem.read_u16(ring_off) as usize;

    let desc_base = ctx.rx_queue.desc_gpa as usize;
    let mut capacity = 0usize;
    let mut idx = head_idx;

    for _ in 0..q_size {
        let d_off = desc_base + idx * 16;
        let Some(desc_slice) = ctx.guest_mem.slice(d_off, 16) else {
            break;
        };
        let len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());
        let next = u16::from_le_bytes(desc_slice[14..16].try_into().unwrap());

        // WRITE flag = device-writable (host->guest).
        if flags & 2 != 0 {
            capacity += len;
        }
        if flags & 1 == 0 {
            break;
        }
        idx = next as usize;
    }

    capacity
}

/// Injects a raw vsock packet into the guest RX queue.
/// Returns the number of bytes written, or 0 on failure.
/// Only commits the used ring entry if the ENTIRE packet was written.
fn inject_packet(ctx: &VsockMuxerContext, packet: &[u8], used_idx: &mut u16) -> usize {
    let q_size = ctx.rx_queue.size as usize;
    if q_size == 0 {
        return 0;
    }

    std::sync::atomic::fence(Ordering::Acquire);
    let avail_idx = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
    if *used_idx == avail_idx {
        return 0;
    }

    let ring_off = ctx.rx_queue.avail_gpa as usize + 4 + 2 * ((*used_idx as usize) % q_size);
    let head_idx = ctx.guest_mem.read_u16(ring_off) as usize;

    // Walk descriptor chain and copy packet data.
    let desc_base = ctx.rx_queue.desc_gpa as usize;
    let mut written = 0;
    let mut idx = head_idx;

    for _ in 0..q_size {
        let d_off = desc_base + idx * 16;
        let Some(desc_slice) = ctx.guest_mem.slice(d_off, 16) else {
            break;
        };

        let addr_gpa = u64::from_le_bytes(desc_slice[0..8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());
        let next = u16::from_le_bytes(desc_slice[14..16].try_into().unwrap());

        if flags & 2 != 0 && len > 0 {
            // SAFETY: descriptor buffers are device-owned during injection.
            let Some(buf) = (unsafe { ctx.guest_mem.slice_mut(addr_gpa, len) }) else {
                continue;
            };
            let remaining = packet.len().saturating_sub(written);
            let to_write = remaining.min(len);
            buf[..to_write].copy_from_slice(&packet[written..written + to_write]);
            written += to_write;
        }

        if flags & 1 == 0 || written >= packet.len() {
            break;
        }
        idx = next as usize;
    }

    // Only commit if the ENTIRE packet was written. A partial vsock
    // packet is corrupted -- the guest will drop it and the connection
    // stalls. Better to not commit and retry later.
    if written < packet.len() {
        return 0;
    }

    // Update RX used ring.
    let used_entry_off = ctx.rx_queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
    ctx.guest_mem.write_u32(used_entry_off, head_idx as u32);
    ctx.guest_mem.write_u32(used_entry_off + 4, written as u32);

    std::sync::atomic::fence(Ordering::Release);
    *used_idx = used_idx.wrapping_add(1);
    ctx.guest_mem
        .write_u16(ctx.rx_queue.used_gpa as usize + 2, *used_idx);

    written
}

// ---------------------------------------------------------------------------
// Packet builders (identical to vsock_rx_worker)
// ---------------------------------------------------------------------------

/// Builds a vsock OP_RW packet from raw data.
fn build_rw_packet(
    conn_id: VsockConnectionId,
    guest_cid: u64,
    data: &[u8],
    fwd_cnt: u32,
) -> Vec<u8> {
    let mut hdr = VsockHeader::new(
        VsockAddr::host(conn_id.host_port),
        VsockAddr::new(guest_cid, conn_id.guest_port),
        VsockOp::Rw,
    );
    hdr.len = data.len() as u32;
    hdr.buf_alloc = TX_BUFFER_SIZE;
    hdr.fwd_cnt = fwd_cnt;

    let hdr_bytes = hdr.to_bytes();
    let mut pkt = Vec::with_capacity(VsockHeader::SIZE + data.len());
    pkt.extend_from_slice(&hdr_bytes[..VsockHeader::SIZE]);
    pkt.extend_from_slice(data);
    pkt
}

/// Builds a control packet (no payload).
fn build_control_packet(
    conn_id: VsockConnectionId,
    guest_cid: u64,
    op: VsockOp,
    fwd_cnt: u32,
    flags: u32,
) -> Vec<u8> {
    let mut hdr = VsockHeader::new(
        VsockAddr::host(conn_id.host_port),
        VsockAddr::new(guest_cid, conn_id.guest_port),
        op,
    );
    hdr.buf_alloc = TX_BUFFER_SIZE;
    hdr.fwd_cnt = fwd_cnt;
    hdr.flags = flags;
    hdr.to_bytes().to_vec()
}

/// Writes a 44-byte VsockHeader (OP_RW) directly into a guest descriptor
/// buffer. Returns `VsockHeader::SIZE` on success, 0 if buffer too small.
fn write_inline_vsock_header(
    buf: &mut [u8],
    conn: &VsockInlineConn,
    payload_len: usize,
    fwd_cnt: u32,
) -> usize {
    if buf.len() < VsockHeader::SIZE {
        return 0;
    }
    buf[0..8].copy_from_slice(&2u64.to_le_bytes()); // src_cid = HOST
    buf[8..16].copy_from_slice(&conn.guest_cid.to_le_bytes()); // dst_cid
    buf[16..20].copy_from_slice(&conn.conn_id.host_port.to_le_bytes()); // src_port
    buf[20..24].copy_from_slice(&conn.conn_id.guest_port.to_le_bytes()); // dst_port
    buf[24..28].copy_from_slice(&(payload_len as u32).to_le_bytes()); // len
    buf[28..30].copy_from_slice(&1u16.to_le_bytes()); // socket_type = STREAM
    buf[30..32].copy_from_slice(&(VsockOp::Rw as u16).to_le_bytes()); // op
    buf[32..36].copy_from_slice(&0u32.to_le_bytes()); // flags
    buf[36..40].copy_from_slice(&TX_BUFFER_SIZE.to_le_bytes()); // buf_alloc
    buf[40..44].copy_from_slice(&fwd_cnt.to_le_bytes()); // fwd_cnt
    VsockHeader::SIZE
}

// ---------------------------------------------------------------------------
// Inline connection polling (identical to vsock_rx_worker)
// ---------------------------------------------------------------------------

/// Polls all inline (promoted) connections, reading from host TCP sockets
/// directly into guest descriptor buffers. This is the zero-socketpair
/// fast path for port forwarding.
fn poll_vsock_inline_conns(
    ctx: &VsockMuxerContext,
    inline_conns: &mut Vec<VsockInlineConn>,
    used_idx: &mut u16,
    batch_count: &mut u16,
    batch_start: &mut Option<Instant>,
) {
    let q_size = ctx.rx_queue.size as usize;
    if q_size == 0 {
        return;
    }

    // Prune closed connections.
    inline_conns.retain(|c| !c.host_eof);

    for conn in inline_conns.iter_mut() {
        if (*batch_count as usize) >= BATCH_SIZE {
            break;
        }

        // 1. Check credit -- single source of truth from the manager.
        let (credit, fwd_cnt) = {
            let Ok(mgr) = ctx.vsock_mgr.lock() else {
                continue;
            };
            let Some(c) = mgr.get(&conn.conn_id) else {
                conn.host_eof = true;
                continue;
            };
            (c.peer_avail_credit(), c.fwd_cnt.0)
        };
        if credit == 0 {
            continue;
        }

        // 2. Check descriptor capacity.
        let desc_cap = peek_rx_capacity(ctx, *used_idx);
        if desc_cap <= VsockHeader::SIZE {
            // Flush interrupt so guest can refill, then backoff.
            if *batch_count > 0 {
                trigger_irq(ctx);
                *batch_count = 0;
                *batch_start = None;
            }
            std::thread::sleep(DESCRIPTOR_BACKOFF);
            break;
        }
        let max_payload = (desc_cap - VsockHeader::SIZE)
            .min(credit)
            .min(MAX_READ_SIZE);

        // 3. Pop descriptor from avail ring.
        std::sync::atomic::fence(Ordering::Acquire);
        let avail_idx = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
        if *used_idx == avail_idx {
            if *batch_count > 0 {
                trigger_irq(ctx);
                *batch_count = 0;
                *batch_start = None;
            }
            break;
        }

        let ring_off = ctx.rx_queue.avail_gpa as usize + 4 + 2 * ((*used_idx as usize) % q_size);
        let head_idx = ctx.guest_mem.read_u16(ring_off) as usize;

        let d_off = ctx.rx_queue.desc_gpa as usize + head_idx * 16;
        let Some(desc_slice) = ctx.guest_mem.slice(d_off, 16) else {
            continue;
        };
        let addr_gpa = u64::from_le_bytes(desc_slice[0..8].try_into().unwrap()) as usize;
        let buf_len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());

        if flags & 2 == 0 || buf_len < VsockHeader::SIZE + 1 {
            continue;
        }

        // SAFETY: descriptor buffers are device-owned during injection.
        let Some(buf) = (unsafe { ctx.guest_mem.slice_mut(addr_gpa, buf_len) }) else {
            continue;
        };

        // 4. Read TCP payload directly into guest buffer at offset 44.
        let payload_limit = max_payload.min(buf_len - VsockHeader::SIZE);
        let payload_buf = &mut buf[VsockHeader::SIZE..VsockHeader::SIZE + payload_limit];
        match conn.stream.read(payload_buf) {
            Ok(0) => {
                // EOF -- send OP_SHUTDOWN.
                conn.host_eof = true;
                let pkt = build_control_packet(
                    conn.conn_id,
                    conn.guest_cid,
                    VsockOp::Shutdown,
                    fwd_cnt,
                    3, // VIRTIO_VSOCK_SHUTDOWN_RCV | SEND
                );
                if inject_packet(ctx, &pkt, used_idx) > 0 {
                    *batch_count += 1;
                    if batch_start.is_none() {
                        *batch_start = Some(Instant::now());
                    }
                }
            }
            Ok(n) => {
                // 5. Write 44-byte VsockHeader inline.
                write_inline_vsock_header(buf, conn, n, fwd_cnt);
                let total = VsockHeader::SIZE + n;

                // 6. Update used ring.
                let used_entry_off =
                    ctx.rx_queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
                ctx.guest_mem.write_u32(used_entry_off, head_idx as u32);
                ctx.guest_mem.write_u32(used_entry_off + 4, total as u32);

                std::sync::atomic::fence(Ordering::Release);
                *used_idx = used_idx.wrapping_add(1);
                ctx.guest_mem
                    .write_u16(ctx.rx_queue.used_gpa as usize + 2, *used_idx);

                // 7. Update credit in the manager (single source of truth).
                if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                    if let Some(c) = mgr.get_mut(&conn.conn_id) {
                        c.record_rx(n as u32);
                    }
                }

                *batch_count += 1;
                if batch_start.is_none() {
                    *batch_start = Some(Instant::now());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                conn.host_eof = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Control packet drain (identical to vsock_rx_worker)
// ---------------------------------------------------------------------------

/// Drains pending control packets (REQUEST, RESPONSE, CREDIT_UPDATE, RESET)
/// from the backend_rxq. OP_RW is NOT drained here -- data injection is
/// handled by the kqueue data path.
fn drain_control_packets(
    ctx: &VsockMuxerContext,
    used_idx: &mut u16,
    batch_count: &mut u16,
    batch_start: &mut Option<Instant>,
) {
    loop {
        let entry = {
            let Ok(mut mgr) = ctx.vsock_mgr.lock() else {
                return;
            };
            // Only drain non-RW ops here. RW is handled by kqueue data path.
            let Some(&id) = mgr.backend_rxq.front() else {
                return;
            };
            let Some(conn) = mgr.get(&id) else {
                mgr.backend_rxq.pop_front();
                continue;
            };

            let op = conn.rx_queue.peek();
            if op == 0 || op == RxOps::RW {
                // RW handled by kqueue path, or empty -- skip.
                mgr.backend_rxq.pop_front();
                continue;
            }

            mgr.backend_rxq.pop_front();
            Some((id, op))
        };

        let Some((conn_id, op)) = entry else { return };

        let pkt = {
            let Ok(mut mgr) = ctx.vsock_mgr.lock() else {
                return;
            };
            let Some(conn) = mgr.get_mut(&conn_id) else {
                continue;
            };
            conn.rx_queue.dequeue(); // Consume the op.

            match op {
                RxOps::REQUEST => {
                    build_control_packet(conn_id, conn.guest_cid, VsockOp::Request, 0, 0)
                }
                RxOps::RESPONSE => {
                    conn.connect = true;
                    build_control_packet(
                        conn_id,
                        conn.guest_cid,
                        VsockOp::Response,
                        conn.fwd_cnt.0,
                        0,
                    )
                }
                RxOps::CREDIT_UPDATE => {
                    let pkt = build_control_packet(
                        conn_id,
                        conn.guest_cid,
                        VsockOp::CreditUpdate,
                        conn.fwd_cnt.0,
                        0,
                    );
                    conn.mark_credit_sent();
                    pkt
                }
                RxOps::RESET => {
                    let pkt = build_control_packet(conn_id, conn.guest_cid, VsockOp::Rst, 0, 0);
                    mgr.remove(&conn_id);
                    pkt
                }
                _ => continue,
            }
        };

        if inject_packet(ctx, &pkt, used_idx) > 0 {
            *batch_count += 1;
            if batch_start.is_none() {
                *batch_start = Some(Instant::now());
            }

            // Fire injected_notify AFTER successful injection for REQUEST.
            if op == RxOps::REQUEST {
                if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                    if let Some(conn) = mgr.get_mut(&conn_id) {
                        if let Some(tx) = conn.injected_notify.take() {
                            let _ = tx.send(());
                        }
                    }
                }
            }
        }

        // Re-enqueue if more ops pending.
        if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
            if let Some(conn) = mgr.get(&conn_id) {
                if conn.rx_queue.pending() {
                    mgr.backend_rxq.push_back(conn_id);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Coalescing flush helper
// ---------------------------------------------------------------------------

/// Flushes interrupt if coalescing timeout has elapsed.
fn flush_if_timeout(
    ctx: &VsockMuxerContext,
    batch_count: &mut u16,
    batch_start: &mut Option<Instant>,
    old_used: &mut u16,
    used_idx: u16,
) {
    if *batch_count > 0 {
        if let Some(start) = *batch_start {
            if start.elapsed() >= COALESCE_TIMEOUT {
                write_avail_event_rx(ctx, used_idx);
                // Unconditional IRQ for latency-bound timeout: if the
                // coalescing timer fires, we must deliver promptly.
                trigger_irq(ctx);
                *batch_count = 0;
                *batch_start = None;
                *old_used = used_idx;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TX queue processing (guest -> host)
// ---------------------------------------------------------------------------

/// Processes pending TX descriptors from the guest TX virtqueue.
///
/// Reads the avail ring via GuestMemWriter, walks each descriptor chain to
/// extract VsockHeader + payload, and dispatches based on the operation code.
/// After processing, updates the TX used ring and fires a TX completion IRQ.
///
/// Returns the number of descriptors processed.
fn process_tx_queue(
    ctx: &VsockMuxerContext,
    last_avail_idx_tx: &mut usize,
    rx_used_idx: &mut u16,
    rx_batch_count: &mut u16,
    rx_batch_start: &mut Option<Instant>,
) -> usize {
    let q_size = ctx.tx_queue.size as usize;
    if q_size == 0 {
        return 0;
    }

    let desc_gpa = ctx.tx_queue.desc_gpa as usize;
    let avail_gpa = ctx.tx_queue.avail_gpa as usize;
    let used_gpa = ctx.tx_queue.used_gpa as usize;

    // Read current avail index from guest memory.
    std::sync::atomic::fence(Ordering::Acquire);
    let avail_idx = ctx.guest_mem.read_u16(avail_gpa + 2) as usize;

    let mut current_avail = *last_avail_idx_tx;
    let mut processed = 0usize;

    while current_avail != avail_idx {
        // Read head descriptor index from avail ring.
        let ring_off = avail_gpa + 4 + 2 * (current_avail % q_size);
        let head_idx = ctx.guest_mem.read_u16(ring_off) as usize;

        // Walk the descriptor chain to collect the full packet data.
        // TX descriptors are read-only (guest->host data).
        let mut packet_data = Vec::new();
        let mut idx = head_idx;

        for _ in 0..q_size {
            let d_off = desc_gpa + idx * 16;
            let Some(desc_slice) = ctx.guest_mem.slice(d_off, 16) else {
                break;
            };

            let addr_gpa = u64::from_le_bytes(desc_slice[0..8].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());
            let next = u16::from_le_bytes(desc_slice[14..16].try_into().unwrap());

            // TX descriptors are read-only (no WRITE flag) -- guest-produced data.
            if flags & 2 == 0 {
                if let Some(data) = ctx.guest_mem.slice(addr_gpa, len) {
                    packet_data.extend_from_slice(data);
                }
            }

            if flags & 1 == 0 {
                break;
            }
            idx = next as usize;
        }

        // Parse VsockHeader (44 bytes) and dispatch.
        if packet_data.len() >= VsockHeader::SIZE {
            if let Some(hdr) = VsockHeader::from_bytes(&packet_data[..VsockHeader::SIZE]) {
                let payload = &packet_data[VsockHeader::SIZE..];
                dispatch_tx_packet(
                    ctx,
                    &hdr,
                    payload,
                    rx_used_idx,
                    rx_batch_count,
                    rx_batch_start,
                );
            }
        } else {
            tracing::warn!(
                "vsock-muxer: TX packet too short ({} bytes < {} header), skipping",
                packet_data.len(),
                VsockHeader::SIZE,
            );
        }

        // Update TX used ring.
        let used_idx_val = ctx.guest_mem.read_u16(used_gpa + 2);
        let used_entry = used_gpa + 4 + ((used_idx_val as usize) % q_size) * 8;
        ctx.guest_mem.write_u32(used_entry, head_idx as u32);
        ctx.guest_mem
            .write_u32(used_entry + 4, packet_data.len() as u32);

        std::sync::atomic::fence(Ordering::Release);
        let new_used = used_idx_val.wrapping_add(1);
        ctx.guest_mem.write_u16(used_gpa + 2, new_used);

        // Write avail_event in the TX used ring so the guest knows when
        // to kick again (EVENT_IDX suppression for TX queue).
        let avail_event_off = used_gpa + 4 + 8 * q_size;
        ctx.guest_mem
            .write_u16(avail_event_off, (current_avail + 1) as u16);

        current_avail += 1;
        processed += 1;
    }

    *last_avail_idx_tx = current_avail;

    // Fire TX completion IRQ if any descriptors were processed.
    if processed > 0 {
        if let Ok(mut s) = ctx.mmio_state.write() {
            s.trigger_interrupt(1); // INT_VRING
        }
        let _ = (ctx.irq_callback)(ctx.irq, true);
        (ctx.exit_vcpus)();
    }

    processed
}

/// Dispatches a single TX packet based on the VsockHeader operation code.
///
/// Uses the `VsockHostConnections` trait methods on the manager for
/// consistent connection state management. Also handles RX-side effects:
/// CreditRequest from the guest triggers an immediate CreditUpdate
/// injection, and OP_RESPONSE is processed so credit info is available.
fn dispatch_tx_packet(
    ctx: &VsockMuxerContext,
    hdr: &VsockHeader,
    payload: &[u8],
    rx_used_idx: &mut u16,
    rx_batch_count: &mut u16,
    rx_batch_start: &mut Option<Instant>,
) {
    // Copy packed fields to locals to avoid unaligned reference UB.
    let src_cid = { hdr.src_cid };
    let dst_cid = { hdr.dst_cid };
    let src_port = { hdr.src_port };
    let dst_port = { hdr.dst_port };
    let buf_alloc = { hdr.buf_alloc };
    let fwd_cnt = { hdr.fwd_cnt };

    match hdr.operation() {
        Some(VsockOp::Request) => {
            tracing::debug!(
                "vsock-muxer TX: OP_REQUEST src={}:{} dst={}:{}",
                src_cid,
                src_port,
                dst_cid,
                dst_port,
            );
            // Guest-initiated connections are not supported in the
            // host-initiated vsock model. Log and ignore.
        }
        Some(VsockOp::Response) => {
            // Guest accepted a host-initiated connection.
            tracing::info!(
                "vsock-muxer TX: OP_RESPONSE -- connection established \
                 (guest_port={}, host_port={}, buf_alloc={}, fwd_cnt={})",
                src_port,
                dst_port,
                buf_alloc,
                fwd_cnt,
            );
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                mgr.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                mgr.mark_connected(src_port, dst_port);
            }
        }
        Some(VsockOp::Rw) => {
            // Guest sends data. Write payload to the host socketpair fd.
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                mgr.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                if let Some(fd) = mgr.fd_for(src_port, dst_port) {
                    if !payload.is_empty() {
                        // SAFETY: fd is a valid connected socket from the manager.
                        let written = unsafe {
                            libc::write(fd, payload.as_ptr().cast::<libc::c_void>(), payload.len())
                        };
                        if written > 0 {
                            tracing::debug!(
                                "vsock-muxer TX: OP_RW guest_port={} host_port={} -> fd {fd}, {} bytes",
                                src_port,
                                dst_port,
                                written,
                            );
                            mgr.advance_fwd_cnt(src_port, dst_port, written as u32);
                        } else if written < 0 {
                            tracing::warn!(
                                "vsock-muxer: write to fd {fd} failed: {}",
                                std::io::Error::last_os_error(),
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        "vsock-muxer TX: OP_RW no host fd for guest_port={} host_port={}",
                        src_port,
                        dst_port,
                    );
                }
            }
        }
        Some(VsockOp::Shutdown | VsockOp::Rst) => {
            tracing::debug!(
                "vsock-muxer TX: connection closed guest_port={} host_port={}",
                src_port,
                dst_port,
            );
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                mgr.remove_connection(src_port, dst_port);
            }
        }
        Some(VsockOp::CreditUpdate) => {
            tracing::trace!(
                "vsock-muxer TX: OP_CREDIT_UPDATE guest_port={} host_port={} \
                 buf_alloc={} fwd_cnt={}",
                src_port,
                dst_port,
                buf_alloc,
                fwd_cnt,
            );
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                mgr.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
            }
        }
        Some(VsockOp::CreditRequest) => {
            // Guest asks us to send our credit state. Enqueue a
            // CreditUpdate on the RX path so it gets injected this cycle.
            tracing::trace!(
                "vsock-muxer TX: OP_CREDIT_REQUEST guest_port={} host_port={}",
                src_port,
                dst_port,
            );
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                mgr.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                mgr.enqueue_credit_update(src_port, dst_port);
            }
            // Immediately drain the freshly-enqueued CreditUpdate so the
            // guest sees it without waiting for the next loop iteration.
            drain_control_packets(ctx, rx_used_idx, rx_batch_count, rx_batch_start);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Main muxer event loop
// ---------------------------------------------------------------------------

/// Main loop for the unified vsock muxer thread.
///
/// Handles both TX processing (guest->host) and RX injection (host->guest)
/// in a single kqueue-based event loop. The vCPU signals TX readiness via
/// `tx_kick_fd` (a pipe). Host socketpair fds are registered dynamically
/// as connections come and go.
pub fn vsock_muxer_loop(ctx: VsockMuxerContext) {
    tracing::info!(
        "vsock-muxer started (rx_queue_size={}, tx_queue_size={})",
        ctx.rx_queue.size,
        ctx.tx_queue.size,
    );

    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        tracing::error!(
            "vsock-muxer: kqueue failed: {}",
            std::io::Error::last_os_error(),
        );
        return;
    }

    // Register the TX kick pipe in kqueue.
    let kick_ev = libc::kevent {
        ident: ctx.tx_kick_fd as usize,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: 0,
        data: 0,
        udata: TX_KICK_TOKEN as *mut libc::c_void,
    };
    let ret = unsafe {
        libc::kevent(
            kq,
            &raw const kick_ev,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if ret < 0 {
        tracing::error!(
            "vsock-muxer: failed to register tx_kick_fd in kqueue: {}",
            std::io::Error::last_os_error(),
        );
        unsafe { libc::close(kq) };
        return;
    }

    // Muxer state.
    let mut rx_used_idx = ctx.guest_mem.read_u16(ctx.rx_queue.used_gpa as usize + 2);
    let mut rx_old_used = rx_used_idx;
    let mut rx_batch_count: u16 = 0;
    let mut rx_batch_start: Option<Instant> = None;
    let mut last_avail_idx_tx: usize =
        ctx.guest_mem.read_u16(ctx.tx_queue.avail_gpa as usize + 2) as usize;

    let mut registered_fds: Vec<i32> = Vec::new();
    let mut read_buf = vec![0u8; MAX_READ_SIZE];
    let mut inline_conns: Vec<VsockInlineConn> = Vec::new();
    let mut kick_drain_buf = [0u8; 64];

    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: POLL_TIMEOUT.as_nanos() as i64,
    };
    let zero_timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    loop {
        if !ctx.running.load(Ordering::Relaxed) {
            break;
        }

        // ---- Step 1: Accept allocation requests (non-blocking) ----
        while let Ok(req) = ctx.alloc_rx.try_recv() {
            if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                let (id, notify_rx) = mgr.allocate(req.guest_port, req.guest_cid, req.internal_fd);
                let _ = req.reply_tx.send((id, notify_rx));
            }
        }

        // ---- Step 2: Accept inline connections (non-blocking) ----
        while let Ok(conn) = ctx.inline_conn_rx.try_recv() {
            tracing::info!(
                "vsock-muxer: inline conn added: host_port={} guest_port={}",
                conn.conn_id.host_port,
                conn.conn_id.guest_port,
            );
            inline_conns.push(conn);
        }

        // ---- Step 3: Dynamic fd registration ----
        // Check for new/removed connections and update kqueue filters.
        {
            let fds: Vec<(VsockConnectionId, i32)> = ctx
                .vsock_mgr
                .lock()
                .map(|mgr| mgr.connected_fds())
                .unwrap_or_default();

            let current_fds: Vec<i32> = fds.iter().map(|(_, fd)| *fd).collect();

            // Register new fds.
            for &fd in &current_fds {
                if !registered_fds.contains(&fd) {
                    let ev = libc::kevent {
                        ident: fd as usize,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD | libc::EV_ENABLE,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut(),
                    };
                    unsafe {
                        libc::kevent(
                            kq,
                            &raw const ev,
                            1,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null(),
                        );
                    }
                }
            }

            // Remove stale fds.
            for &fd in &registered_fds {
                if !current_fds.contains(&fd) {
                    let ev = libc::kevent {
                        ident: fd as usize,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_DELETE,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut(),
                    };
                    unsafe {
                        libc::kevent(
                            kq,
                            &raw const ev,
                            1,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null(),
                        );
                    }
                }
            }

            if registered_fds.len() != current_fds.len() {
                tracing::info!(
                    "vsock-muxer: fd registration changed: {} -> {}",
                    registered_fds.len(),
                    current_fds.len(),
                );
            }
            registered_fds = current_fds;
        }

        // ---- Step 4: Drain control packets ----
        drain_control_packets(
            &ctx,
            &mut rx_used_idx,
            &mut rx_batch_count,
            &mut rx_batch_start,
        );

        // ---- Step 4b: Poll inline connections ----
        if !inline_conns.is_empty() {
            poll_vsock_inline_conns(
                &ctx,
                &mut inline_conns,
                &mut rx_used_idx,
                &mut rx_batch_count,
                &mut rx_batch_start,
            );
        }

        // ---- Step 5: kqueue wait ----
        // Use zero timeout when inline conns are active AND there are RX
        // descriptors available, to avoid adding latency to inline reads.
        let effective_timeout = if !inline_conns.is_empty()
            && peek_rx_capacity(&ctx, rx_used_idx) > VsockHeader::SIZE
        {
            &zero_timeout
        } else {
            &timeout
        };
        let mut events = [libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        }; 16];
        let nev = unsafe {
            libc::kevent(
                kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                i32::try_from(events.len()).unwrap_or(16),
                std::ptr::from_ref::<libc::timespec>(effective_timeout),
            )
        };

        if nev <= 0 {
            // Timeout or error -- check coalescing.
            flush_if_timeout(
                &ctx,
                &mut rx_batch_count,
                &mut rx_batch_start,
                &mut rx_old_used,
                rx_used_idx,
            );
            continue;
        }

        // ---- Step 6: Process TX kick ----
        // Check if any event is the TX kick pipe. If so, drain the pipe
        // (coalescing multiple kicks) and process the full TX queue.
        let mut had_tx_kick = false;
        for event in events.iter().take(nev as usize) {
            if event.udata as usize == TX_KICK_TOKEN {
                had_tx_kick = true;
                break;
            }
        }

        if had_tx_kick {
            // Drain all bytes from the kick pipe (coalesce kicks).
            loop {
                let n = unsafe {
                    libc::read(
                        ctx.tx_kick_fd,
                        kick_drain_buf.as_mut_ptr().cast::<libc::c_void>(),
                        kick_drain_buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
            }

            // Process the entire TX queue.
            let tx_count = process_tx_queue(
                &ctx,
                &mut last_avail_idx_tx,
                &mut rx_used_idx,
                &mut rx_batch_count,
                &mut rx_batch_start,
            );
            if tx_count > 0 {
                tracing::trace!("vsock-muxer: processed {} TX descriptors", tx_count);
            }
        }

        // ---- Step 7: Process readable socketpair fds (RX data injection) ----
        // Skip data injection if the RX queue is running low (>3/4 consumed).
        let q_size = ctx.rx_queue.size as usize;
        let avail_idx_snap = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
        let q_free = avail_idx_snap.wrapping_sub(rx_used_idx) as usize;
        let q_backpressure = q_free < q_size / 4;

        if q_backpressure {
            // Queue running low -- fire IRQ to wake guest rx_work so it
            // returns descriptors, then skip data injection this cycle.
            if rx_batch_count > 0 {
                rx_batch_count = 0;
                rx_batch_start = None;
                rx_old_used = rx_used_idx;
            }
            write_avail_event_rx(&ctx, rx_used_idx);
            trigger_irq(&ctx);
        } else {
            // Process each readable socketpair fd from kqueue events.
            for event in events.iter().take(nev as usize) {
                // Skip the TX kick pipe event (already handled above).
                if event.udata as usize == TX_KICK_TOKEN {
                    continue;
                }

                #[allow(clippy::cast_possible_wrap)]
                let fd = event.ident as i32;

                // Find which connection this fd belongs to.
                let conn_info = {
                    let Ok(mgr) = ctx.vsock_mgr.lock() else {
                        continue;
                    };
                    mgr.connected_fds()
                        .into_iter()
                        .find(|(_, f)| *f == fd)
                        .map(|(id, _)| id)
                };
                let Some(conn_id) = conn_info else { continue };

                // Read in a loop until EAGAIN, batch limit, or credit exhaustion.
                // Credit is the natural limiter: each connection injects up to
                // peer_avail_credit bytes per cycle.
                for _ in 0..BATCH_SIZE {
                    if rx_batch_count as usize >= BATCH_SIZE {
                        break;
                    }

                    // Check credit.
                    let (credit, guest_cid, fwd_cnt) = {
                        let Ok(mgr) = ctx.vsock_mgr.lock() else {
                            break;
                        };
                        let Some(conn) = mgr.get(&conn_id) else {
                            break;
                        };
                        (conn.peer_avail_credit(), conn.guest_cid, conn.fwd_cnt.0)
                    };

                    if credit == 0 {
                        // Send CreditRequest to solicit a CreditUpdate from
                        // the guest. Without this, the guest only sends
                        // CreditUpdate when the application calls recvmsg().
                        // CreditRequest triggers the guest kernel to respond
                        // with OP_CREDIT_UPDATE unconditionally.
                        let pkt = {
                            let Ok(mgr) = ctx.vsock_mgr.lock() else {
                                break;
                            };
                            let Some(conn) = mgr.get(&conn_id) else {
                                break;
                            };
                            build_control_packet(
                                conn_id,
                                conn.guest_cid,
                                VsockOp::CreditRequest,
                                conn.fwd_cnt.0,
                                0,
                            )
                        };
                        if inject_packet(&ctx, &pkt, &mut rx_used_idx) > 0 {
                            write_avail_event_rx(&ctx, rx_used_idx);
                            trigger_irq(&ctx);
                            rx_old_used = rx_used_idx;
                        }
                        break;
                    }

                    // Pre-check descriptor capacity.
                    let desc_cap = peek_rx_capacity(&ctx, rx_used_idx);
                    if desc_cap <= VsockHeader::SIZE {
                        // No room for even a header -- descriptor exhaustion.
                        if rx_batch_count > 0 {
                            write_avail_event_rx(&ctx, rx_used_idx);
                            maybe_notify(&ctx, rx_old_used, rx_used_idx);
                            rx_batch_count = 0;
                            rx_batch_start = None;
                            rx_old_used = rx_used_idx;
                        }
                        std::thread::sleep(DESCRIPTOR_BACKOFF);
                        break;
                    }
                    let max_payload = desc_cap - VsockHeader::SIZE;
                    let max_read = credit.min(max_payload).min(MAX_READ_SIZE);
                    if max_read == 0 {
                        break;
                    }

                    let n = unsafe {
                        libc::read(fd, read_buf.as_mut_ptr().cast::<libc::c_void>(), max_read)
                    };

                    if n <= 0 {
                        if n == 0 {
                            // EOF -- send OP_SHUTDOWN to the guest.
                            let pkt = {
                                let Ok(mgr) = ctx.vsock_mgr.lock() else {
                                    break;
                                };
                                let Some(conn) = mgr.get(&conn_id) else {
                                    break;
                                };
                                build_control_packet(
                                    conn_id,
                                    conn.guest_cid,
                                    VsockOp::Shutdown,
                                    conn.fwd_cnt.0,
                                    3, // VIRTIO_VSOCK_SHUTDOWN_RCV | SEND
                                )
                            };
                            if inject_packet(&ctx, &pkt, &mut rx_used_idx) > 0 {
                                rx_batch_count += 1;
                                if rx_batch_start.is_none() {
                                    rx_batch_start = Some(Instant::now());
                                }
                            }
                        }
                        break;
                    }

                    let data = &read_buf[..n as usize];
                    let pkt = build_rw_packet(conn_id, guest_cid, data, fwd_cnt);

                    let written = inject_packet(&ctx, &pkt, &mut rx_used_idx);
                    if written > 0 {
                        // Record credit ONLY after successful full injection.
                        if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                            if let Some(conn) = mgr.get_mut(&conn_id) {
                                conn.record_rx(data.len() as u32);
                                // Detect credit overrun (debug aid).
                                let in_flight = (conn.rx_cnt - conn.peer_fwd_cnt).0;
                                if in_flight > conn.peer_buf_alloc {
                                    tracing::warn!(
                                        "vsock-muxer: CREDIT OVERRUN conn={:?} \
                                         in_flight={} buf_alloc={} data_len={}",
                                        conn_id,
                                        in_flight,
                                        conn.peer_buf_alloc,
                                        data.len(),
                                    );
                                }
                            }
                        }
                        rx_batch_count += 1;
                        if rx_batch_start.is_none() {
                            rx_batch_start = Some(Instant::now());
                        }
                    } else {
                        // Injection failed -- partial write prevented commit.
                        tracing::debug!(
                            "vsock-muxer: inject_packet returned 0, pkt_len={} desc_cap={}",
                            pkt.len(),
                            desc_cap,
                        );
                        if rx_batch_count > 0 {
                            write_avail_event_rx(&ctx, rx_used_idx);
                            maybe_notify(&ctx, rx_old_used, rx_used_idx);
                            rx_batch_count = 0;
                            rx_batch_start = None;
                            rx_old_used = rx_used_idx;
                        }
                        std::thread::sleep(DESCRIPTOR_BACKOFF);
                        break;
                    }

                    // Flush at batch boundary for IRQ coalescing.
                    if rx_batch_count as usize >= BATCH_SIZE {
                        write_avail_event_rx(&ctx, rx_used_idx);
                        maybe_notify(&ctx, rx_old_used, rx_used_idx);
                        rx_batch_count = 0;
                        rx_batch_start = None;
                        rx_old_used = rx_used_idx;
                    }
                }
            }
        }

        // ---- Step 8: End-of-cycle flush ----
        // Flush any pending RX batch. The muxer's TX processing naturally
        // paces RX injection -- credit updates from TX take effect
        // immediately within the same cycle.
        if rx_batch_count > 0 {
            write_avail_event_rx(&ctx, rx_used_idx);
            trigger_irq(&ctx);
            rx_batch_count = 0;
            rx_batch_start = None;
            rx_old_used = rx_used_idx;
        }
    }

    // Final flush before shutdown.
    if rx_batch_count > 0 {
        write_avail_event_rx(&ctx, rx_used_idx);
        maybe_notify(&ctx, rx_old_used, rx_used_idx);
    }
    unsafe { libc::close(kq) };
    tracing::info!("vsock-muxer stopped");
}
