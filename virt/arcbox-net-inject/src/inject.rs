//! Dedicated OS thread for RX frame injection.
//!
//! Supports two input paths:
//!   1. **Channel frames** (`rx`): pre-constructed Ethernet frames from smoltcp/DHCP/DNS.
//!   2. **Inline connections** (`conn_rx`): promoted fast-path TCP sockets that
//!      read directly into guest descriptor buffers (zero intermediate copies).

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::guest_mem::GuestMemWriter;
use crate::inline_conn::{self, InlineConn};
use crate::irq::IrqHandle;
use crate::queue::{self, RxQueueConfig};

/// Maximum frames to inject per batch before checking interrupt thresholds.
///
/// At line-rate (~9 Gbps, 500k fps), a batch of 64 frames means the inject
/// thread fires IRQ ~8000 times/second. Each IRQ entails an MMIO trap and
/// guest-side RX soft-irq processing. Raising to 256 cuts IRQ rate to
/// ~2000/s while the 50 µs `COALESCE_TIMEOUT` still bounds latency.
const BATCH_SIZE: usize = 256;

/// Interrupt coalescing timeout.
///
/// With GSO on the RX path the average frame size is ~30 KiB, so at
/// 10 Gbps we're at ~37 kfps — a 50 µs timeout only batches ~2 frames
/// before firing an IRQ, so the `BATCH_SIZE` ceiling is never reached.
/// 200 µs raises the effective batch to ~7 frames without noticeably
/// hurting ACK latency (Linux NAPI poll is typically 200 µs anyway).
const COALESCE_TIMEOUT: Duration = Duration::from_micros(200);

/// Backoff duration when RX descriptors are exhausted.
const DESCRIPTOR_BACKOFF: Duration = Duration::from_micros(100);

/// Context for the RX injection thread.
pub struct RxInjectThread {
    /// Channel receiving raw Ethernet frames from the producer.
    pub rx: Receiver<Vec<u8>>,
    /// Channel for receiving promoted fast-path connections.
    pub conn_rx: Receiver<InlineConn>,
    /// Guest memory writer (Send + Sync, VM-lifetime pointer).
    pub guest_mem: GuestMemWriter,
    /// RX queue layout (queue index 0 of primary VirtioNet).
    pub queue: RxQueueConfig,
    /// Interrupt delivery handle.
    pub irq: IrqHandle,
    /// MMIO state for setting interrupt_status. Wrapped for &self access.
    /// The inject thread needs to set interrupt_status |= INT_VRING
    /// before firing the GIC SPI.
    pub set_interrupt_status: Arc<dyn Fn() + Send + Sync>,
    /// VM shutdown flag.
    pub running: Arc<AtomicBool>,
}

// SAFETY: All fields are either Send+Sync or raw pointers wrapped
// in GuestMemWriter which is Send+Sync.
unsafe impl Send for RxInjectThread {}

impl RxInjectThread {
    /// Runs the injection loop until `running` is set to false or
    /// the channel is disconnected.
    pub fn run(self) {
        tracing::info!("rx-inject thread started (queue_size={})", self.queue.size);

        let mut used_idx = self.guest_mem.read_u16(self.queue.used_gpa as usize + 2);
        let mut old_used = used_idx;
        let mut inline_conns: Vec<InlineConn> = Vec::new();

        loop {
            if !self.running.load(Ordering::Relaxed) {
                break;
            }

            // Phase 1: Accept new inline connections (non-blocking).
            while let Ok(conn) = self.conn_rx.try_recv() {
                tracing::info!(
                    "inline conn added: {}:{} -> {}:{}",
                    conn.remote_ip,
                    conn.remote_port,
                    conn.guest_ip,
                    conn.guest_port,
                );
                inline_conns.push(conn);
            }

            let mut batch = 0u16;
            let loop_start = Instant::now();

            // Phase 2: Poll inline connections (direct socket -> guest buffer).
            if !inline_conns.is_empty() {
                self.poll_inline_conns(&mut inline_conns, &mut used_idx, &mut batch);
            }

            // Phase 3: Drain channel frames (smoltcp/DHCP/DNS).
            // Use the remaining coalescing timeout after inline polling.
            let elapsed = loop_start.elapsed();
            let remaining = COALESCE_TIMEOUT.saturating_sub(elapsed);

            while (batch as usize) < BATCH_SIZE {
                // Use the remaining timeout for the first recv, then zero
                // for subsequent ones to drain without blocking.
                let timeout = if batch == 0 && inline_conns.is_empty() {
                    // No inline conns and nothing batched yet — block for
                    // the full coalescing timeout.
                    COALESCE_TIMEOUT
                } else if remaining.is_zero() {
                    // Timeout already consumed by inline polling — try_recv only.
                    Duration::ZERO
                } else {
                    remaining
                };

                let frame = match self.rx.recv_timeout(timeout) {
                    Ok(f) => f,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        tracing::info!("rx-inject: channel disconnected, shutting down");
                        if batch > 0 {
                            self.flush_interrupt(old_used, used_idx);
                        }
                        return;
                    }
                };

                if queue::inject_one_frame(&self.guest_mem, &self.queue, &frame, &mut used_idx) {
                    batch += 1;
                } else {
                    // Descriptor exhaustion: flush interrupt so guest can
                    // process and repost, then backoff.
                    if batch > 0 {
                        self.flush_interrupt(old_used, used_idx);
                        old_used = used_idx;
                        batch = 0;
                    }
                    std::thread::sleep(DESCRIPTOR_BACKOFF);

                    // Retry this frame once.
                    if queue::inject_one_frame(&self.guest_mem, &self.queue, &frame, &mut used_idx)
                    {
                        batch += 1;
                    }
                    // If still fails, frame is lost (TCP retransmit recovers).
                }
            }

            if batch > 0 {
                self.flush_interrupt(old_used, used_idx);
                old_used = used_idx;
            }
        }

        // Final flush on shutdown.
        if old_used != used_idx {
            self.flush_interrupt(old_used, used_idx);
        }

        tracing::info!(
            "rx-inject thread stopped ({} inline conns remaining)",
            inline_conns.len(),
        );
    }

    /// Polls all active inline connections, reading from host sockets
    /// directly into guest descriptor buffers. Connections that reach
    /// EOF or error are marked and pruned at the start of the next call.
    fn poll_inline_conns(
        &self,
        inline_conns: &mut Vec<InlineConn>,
        used_idx: &mut u16,
        batch: &mut u16,
    ) {
        let q_size = self.queue.size as usize;
        if q_size == 0 {
            return;
        }

        // Prune closed connections from previous iteration.
        inline_conns.retain(|c| !c.host_eof);

        for conn in inline_conns.iter_mut() {
            if (*batch as usize) >= BATCH_SIZE {
                break;
            }

            // Check available descriptors.
            std::sync::atomic::fence(Ordering::Acquire);
            let avail_idx = self.guest_mem.read_u16(self.queue.avail_gpa as usize + 2);
            if *used_idx == avail_idx {
                // No descriptors available — stop all inline polling.
                break;
            }

            // Pop the next available descriptor.
            let ring_off = self.queue.avail_gpa as usize + 4 + 2 * ((*used_idx as usize) % q_size);
            let head_idx = self.guest_mem.read_u16(ring_off) as usize;

            // Read descriptor to find a writable buffer large enough for
            // at least the 66-byte header + 1 byte of payload.
            let desc_base = self.queue.desc_gpa as usize;
            let d_off = desc_base + head_idx * 16;
            let Some(desc_slice) = self.guest_mem.slice(d_off, 16) else {
                continue;
            };

            let addr_gpa = u64::from_le_bytes(desc_slice[0..8].try_into().unwrap()) as usize;
            let buf_len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());

            // Descriptor must be device-writable (bit 1) and large enough.
            if flags & 2 == 0 || buf_len < inline_conn::TOTAL_HDR_LEN + 1 {
                continue;
            }

            // SAFETY: VirtIO descriptor buffers are device-owned during
            // injection. The guest will not access them until used_idx
            // advances (after Release fence below).
            let Some(buf) = (unsafe { self.guest_mem.slice_mut(addr_gpa, buf_len) }) else {
                continue;
            };

            // Read payload from host socket directly into guest buffer
            // at offset 66 (after the header region).
            let payload_buf = &mut buf[inline_conn::TOTAL_HDR_LEN..];
            match conn.stream.read(payload_buf) {
                Ok(0) => {
                    tracing::debug!(
                        "inline {}:{}->{}:{} host EOF",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port
                    );
                    conn.host_eof = true;

                    // Inject FIN+ACK so the guest half-closes its receive
                    // side gracefully. Without this, guest writes after
                    // host EOF hit Broken Pipe in tcp_bridge, the conn is
                    // removed, and smoltcp replies with RST on the next
                    // guest TX — surfacing as "connection reset by peer".
                    inline_conn::write_fin_headers(buf, conn);
                    conn.our_seq = conn.our_seq.wrapping_add(1); // FIN consumes 1 SEQ.

                    let used_entry_off =
                        self.queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
                    self.guest_mem.write_u32(used_entry_off, head_idx as u32);
                    self.guest_mem
                        .write_u32(used_entry_off + 4, inline_conn::TOTAL_HDR_LEN as u32);

                    std::sync::atomic::fence(Ordering::Release);
                    *used_idx = used_idx.wrapping_add(1);
                    self.guest_mem
                        .write_u16(self.queue.used_gpa as usize + 2, *used_idx);

                    *batch += 1;
                }
                Ok(n) => {
                    // Write 66-byte header inline (no allocation).
                    inline_conn::write_inline_headers(buf, conn, n);
                    conn.our_seq = conn.our_seq.wrapping_add(n as u32);

                    let total_written = inline_conn::TOTAL_HDR_LEN + n;

                    // Update used ring entry.
                    let used_entry_off =
                        self.queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
                    self.guest_mem.write_u32(used_entry_off, head_idx as u32);
                    self.guest_mem
                        .write_u32(used_entry_off + 4, total_written as u32);

                    // Release fence: ensure descriptor data is visible before
                    // used_idx advances.
                    std::sync::atomic::fence(Ordering::Release);
                    *used_idx = used_idx.wrapping_add(1);
                    self.guest_mem
                        .write_u16(self.queue.used_gpa as usize + 2, *used_idx);

                    *batch += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data ready on this socket — try the next connection.
                }
                Err(e) => {
                    tracing::debug!("inline conn error: {e}");
                    conn.host_eof = true;
                }
            }
        }
    }

    /// Fires interrupt after a batch of injections.
    fn flush_interrupt(&self, _old_used: u16, _used_idx: u16) {
        // Write avail_event for EVENT_IDX.
        let q_size = self.queue.size as usize;
        let avail_event_off = self.queue.used_gpa as usize + 4 + q_size * 8;
        let avail_idx = self.guest_mem.read_u16(self.queue.avail_gpa as usize + 2);
        // Release fence: ensure prior used-ring writes are visible before
        // the guest observes the new avail_event value.
        std::sync::atomic::fence(Ordering::Release);
        self.guest_mem.write_u16(avail_event_off, avail_idx);

        // Unconditionally fire interrupt. EVENT_IDX suppression can be
        // added later once the basic path is validated.
        (self.set_interrupt_status)();
        self.irq.trigger();
    }
}
