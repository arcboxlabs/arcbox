//! Dedicated OS thread for RX frame injection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::guest_mem::GuestMemWriter;
use crate::irq::IrqHandle;
use crate::queue::{self, RxQueueConfig};

/// Maximum frames to inject per batch before checking interrupt thresholds.
const BATCH_SIZE: usize = 64;

/// Interrupt coalescing timeout.
const COALESCE_TIMEOUT: Duration = Duration::from_micros(50);

/// Backoff duration when RX descriptors are exhausted.
const DESCRIPTOR_BACKOFF: Duration = Duration::from_micros(100);

/// Context for the RX injection thread.
pub struct RxInjectThread {
    /// Channel receiving raw Ethernet frames from the producer.
    pub rx: Receiver<Vec<u8>>,
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
        tracing::info!("rx-inject thread started (queue_size={})", self.queue.size,);

        let mut used_idx = self.guest_mem.read_u16(self.queue.used_gpa as usize + 2);
        let mut old_used = used_idx;

        loop {
            if !self.running.load(Ordering::Relaxed) {
                break;
            }

            let mut batch = 0u16;

            // Drain channel up to BATCH_SIZE frames.
            while (batch as usize) < BATCH_SIZE {
                let frame = match self.rx.recv_timeout(COALESCE_TIMEOUT) {
                    Ok(f) => f,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        tracing::info!("rx-inject: channel disconnected, shutting down");
                        // Flush any pending frames.
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

        tracing::info!("rx-inject thread stopped");
    }

    /// Fires interrupt after a batch of injections.
    fn flush_interrupt(&self, _old_used: u16, _used_idx: u16) {
        // Write avail_event for EVENT_IDX.
        let q_size = self.queue.size as usize;
        let avail_event_off = self.queue.used_gpa as usize + 4 + q_size * 8;
        let avail_idx = self.guest_mem.read_u16(self.queue.avail_gpa as usize + 2);
        self.guest_mem.write_u16(avail_event_off, avail_idx);

        // Unconditionally fire interrupt. EVENT_IDX suppression can be
        // added later once the basic path is validated.
        (self.set_interrupt_status)();
        self.irq.trigger();
    }
}
