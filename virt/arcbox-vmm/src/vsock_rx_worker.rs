//! Dedicated I/O worker thread for vsock RX injection (host → guest).
//!
//! Mirrors the `net_rx_worker` architecture: a kqueue-based thread that
//! continuously polls host socketpair fds and injects vsock packets into
//! the guest RX virtqueue. This decouples data transfer from the vCPU
//! run loop, eliminating the 1-10ms polling gap that caused stalls.
//!
//! The worker handles:
//! - **Multiple connections**: dynamically registers/unregisters fds
//! - **Credit flow control**: checks `peer_avail_credit` before reading
//! - **Interrupt coalescing**: batches packets before firing GIC SPI
//! - **Control packets**: REQUEST, RESPONSE, CREDIT_UPDATE, RESET

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arcbox_virtio::vsock::{VsockAddr, VsockHeader, VsockOp};

use crate::blk_worker::GuestMemWriter;
use crate::device::VirtioMmioState;
use crate::irq::Irq;
use crate::vsock_manager::{RxOps, TX_BUFFER_SIZE, VsockConnectionId, VsockConnectionManager};

/// Maximum packets to inject per kqueue wakeup.
const BATCH_SIZE: usize = 64;

/// Maximum bytes to read from a single socket per iteration.
/// Larger reads improve throughput by reducing per-packet overhead.
const MAX_READ_SIZE: usize = 65536;

/// Interrupt coalescing timeout (latency bound).
const COALESCE_TIMEOUT: Duration = Duration::from_micros(50);

/// kqueue poll timeout — bounds the shutdown and new-fd check frequency.
const POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// Backoff when RX descriptors are exhausted.
const DESCRIPTOR_BACKOFF: Duration = Duration::from_micros(100);

/// RX queue layout, captured at DRIVER_OK time.
pub struct VsockRxQueueConfig {
    pub desc_gpa: u64,
    pub avail_gpa: u64,
    pub used_gpa: u64,
    pub size: u16,
}

/// Shared context for the vsock RX worker thread.
pub struct VsockRxWorkerContext {
    pub guest_mem: GuestMemWriter,
    pub rx_queue: VsockRxQueueConfig,
    pub mmio_state: Arc<RwLock<VirtioMmioState>>,
    pub irq_callback: Arc<dyn Fn(Irq, bool) -> crate::error::Result<()> + Send + Sync>,
    pub irq: Irq,
    pub exit_vcpus: Arc<dyn Fn() + Send + Sync>,
    pub vsock_mgr: Arc<Mutex<VsockConnectionManager>>,
    pub running: Arc<AtomicBool>,
}

// SAFETY: All fields are Send+Sync or wrapped in Arc<Mutex/RwLock>.
unsafe impl Send for VsockRxWorkerContext {}

/// Triggers a vsock RX interrupt.
fn trigger_irq(ctx: &VsockRxWorkerContext) {
    if let Ok(mut s) = ctx.mmio_state.write() {
        s.trigger_interrupt(1); // INT_VRING
    }
    let _ = (ctx.irq_callback)(ctx.irq, true);
    (ctx.exit_vcpus)();
}

/// Injects a raw vsock packet into the guest RX queue.
/// Returns true on success.
fn inject_packet(ctx: &VsockRxWorkerContext, packet: &[u8], used_idx: &mut u16) -> bool {
    let q_size = ctx.rx_queue.size as usize;
    if q_size == 0 {
        return false;
    }

    std::sync::atomic::fence(Ordering::Acquire);
    let avail_idx = ctx.guest_mem.read_u16(ctx.rx_queue.avail_gpa as usize + 2);
    if *used_idx == avail_idx {
        return false;
    }

    let ring_off = ctx.rx_queue.avail_gpa as usize + 4 + 2 * ((*used_idx as usize) % q_size);
    let head_idx = ctx.guest_mem.read_u16(ring_off) as usize;

    // Walk descriptor chain.
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

    if written == 0 {
        return false;
    }

    // Update used ring.
    let used_entry_off = ctx.rx_queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
    ctx.guest_mem.write_u32(used_entry_off, head_idx as u32);
    ctx.guest_mem.write_u32(used_entry_off + 4, written as u32);

    std::sync::atomic::fence(Ordering::Release);
    *used_idx = used_idx.wrapping_add(1);
    ctx.guest_mem
        .write_u16(ctx.rx_queue.used_gpa as usize + 2, *used_idx);

    true
}

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

/// Main loop for the vsock RX worker thread.
pub fn vsock_rx_worker_loop(ctx: VsockRxWorkerContext) {
    tracing::info!("vsock-rx worker started (queue_size={})", ctx.rx_queue.size);

    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        tracing::error!(
            "vsock-rx: kqueue failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    let mut used_idx = ctx.guest_mem.read_u16(ctx.rx_queue.used_gpa as usize + 2);
    let mut old_used = used_idx;
    let mut batch_count: u16 = 0;
    let mut batch_start: Option<Instant> = None;
    let mut registered_fds: Vec<i32> = Vec::new();
    let mut read_buf = vec![0u8; MAX_READ_SIZE];

    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: POLL_TIMEOUT.as_nanos() as i64,
    };

    loop {
        if !ctx.running.load(Ordering::Relaxed) {
            break;
        }

        // ----- Dynamic fd registration -----
        // Check for new/removed connections and update kqueue.
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

            registered_fds = current_fds;
        }

        // ----- Also drain control packets (REQUEST, RESPONSE, CREDIT_UPDATE, RESET) -----
        drain_control_packets(&ctx, &mut used_idx, &mut batch_count, &mut batch_start);

        // ----- kqueue wait -----
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
                &raw const timeout,
            )
        };

        if nev <= 0 {
            // Timeout or error — check coalescing.
            flush_if_timeout(
                &ctx,
                &mut batch_count,
                &mut batch_start,
                &mut old_used,
                used_idx,
            );
            continue;
        }

        // ----- Process readable fds -----
        for event in events.iter().take(nev as usize) {
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

            // Read in a loop until EAGAIN or batch limit.
            for _ in 0..BATCH_SIZE {
                if batch_count as usize >= BATCH_SIZE {
                    break;
                }

                // Check credit.
                let (credit, guest_cid, fwd_cnt) = {
                    let Ok(mgr) = ctx.vsock_mgr.lock() else { break };
                    let Some(conn) = mgr.get(&conn_id) else { break };
                    (conn.peer_avail_credit(), conn.guest_cid, conn.fwd_cnt.0)
                };

                if credit == 0 {
                    // Request credit update from guest.
                    let pkt = {
                        let Ok(mgr) = ctx.vsock_mgr.lock() else { break };
                        let Some(conn) = mgr.get(&conn_id) else { break };
                        build_control_packet(
                            conn_id,
                            conn.guest_cid,
                            VsockOp::CreditRequest,
                            conn.fwd_cnt.0,
                            0,
                        )
                    };
                    let _ = inject_packet(&ctx, &pkt, &mut used_idx);
                    break;
                }

                let max_read = credit.min(MAX_READ_SIZE);
                let n = unsafe {
                    libc::read(fd, read_buf.as_mut_ptr().cast::<libc::c_void>(), max_read)
                };

                if n <= 0 {
                    if n == 0 {
                        // EOF — send SHUTDOWN.
                        let pkt = {
                            let Ok(mgr) = ctx.vsock_mgr.lock() else { break };
                            let Some(conn) = mgr.get(&conn_id) else { break };
                            build_control_packet(
                                conn_id,
                                conn.guest_cid,
                                VsockOp::Shutdown,
                                conn.fwd_cnt.0,
                                3,
                            )
                        };
                        if inject_packet(&ctx, &pkt, &mut used_idx) {
                            batch_count += 1;
                            if batch_start.is_none() {
                                batch_start = Some(Instant::now());
                            }
                        }
                    }
                    break; // EAGAIN or error
                }

                let data = &read_buf[..n as usize];
                let pkt = build_rw_packet(conn_id, guest_cid, data, fwd_cnt);

                // Record bytes sent to guest.
                if let Ok(mut mgr) = ctx.vsock_mgr.lock() {
                    if let Some(conn) = mgr.get_mut(&conn_id) {
                        conn.record_rx(data.len() as u32);
                    }
                }

                if inject_packet(&ctx, &pkt, &mut used_idx) {
                    batch_count += 1;
                    if batch_start.is_none() {
                        batch_start = Some(Instant::now());
                    }
                } else {
                    // Descriptor exhaustion — flush and backoff.
                    if batch_count > 0 {
                        trigger_irq(&ctx);
                        batch_count = 0;
                        batch_start = None;
                        old_used = used_idx;
                    }
                    std::thread::sleep(DESCRIPTOR_BACKOFF);
                    break;
                }

                // Flush at batch boundary.
                if batch_count as usize >= BATCH_SIZE {
                    trigger_irq(&ctx);
                    batch_count = 0;
                    batch_start = None;
                    old_used = used_idx;
                }
            }
        }

        // Check coalescing timeout.
        flush_if_timeout(
            &ctx,
            &mut batch_count,
            &mut batch_start,
            &mut old_used,
            used_idx,
        );
    }

    // Final flush.
    if batch_count > 0 {
        trigger_irq(&ctx);
    }
    unsafe { libc::close(kq) };
    tracing::info!("vsock-rx worker stopped");
}

/// Drains pending control packets (REQUEST, RESPONSE, CREDIT_UPDATE, RESET)
/// from the backend_rxq.
fn drain_control_packets(
    ctx: &VsockRxWorkerContext,
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
                // RW handled by kqueue path, or empty — skip.
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
                    let pkt = build_control_packet(conn_id, conn.guest_cid, VsockOp::Request, 0, 0);
                    // Fire injected_notify.
                    if let Some(tx) = conn.injected_notify.take() {
                        let _ = tx.send(());
                    }
                    pkt
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

        if inject_packet(ctx, &pkt, used_idx) {
            *batch_count += 1;
            if batch_start.is_none() {
                *batch_start = Some(Instant::now());
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

/// Flushes interrupt if coalescing timeout has elapsed.
fn flush_if_timeout(
    ctx: &VsockRxWorkerContext,
    batch_count: &mut u16,
    batch_start: &mut Option<Instant>,
    old_used: &mut u16,
    used_idx: u16,
) {
    if *batch_count > 0 {
        if let Some(start) = *batch_start {
            if start.elapsed() >= COALESCE_TIMEOUT {
                trigger_irq(ctx);
                *batch_count = 0;
                *batch_start = None;
                *old_used = used_idx;
            }
        }
    }
}
