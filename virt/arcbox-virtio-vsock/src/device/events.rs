//! The vsock event queue (queue 2).

use arcbox_virtio_core::{QueueConfig, SplitQueue};

use super::VirtioVsock;

/// `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET`: the device lost every connection.
const EVENT_TRANSPORT_RESET: u32 = 0;
/// Size of `struct virtio_vsock_event`.
const EVENT_SIZE: usize = 4;

/// A posted TRANSPORT_RESET, until the guest has handled it.
///
/// The guest resets every connected socket when it handles the event,
/// including one a host connect created a moment earlier, which then hangs
/// on the host side with nothing to tell it. So host→guest injection waits
/// until the guest refills the event buffer: Linux does that right after
/// handling the event, and the avail index moves whether or not it kicks.
pub(super) struct PendingReset {
    /// Guest-physical address of the event queue's `avail.idx`.
    avail_idx: usize,
    posted_at: u16,
}

impl VirtioVsock {
    /// Posts `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` on the event queue
    /// described by `qcfg`.
    ///
    /// The guest driver answers by resetting every connected socket
    /// (ECONNRESET) and re-reading the CID; listening sockets stay. This is
    /// how a device that comes back without its host-side connections, as
    /// after a checkpoint restore, tells the guest. Returns whether the event
    /// was published; the caller raises the used-buffer interrupt. `false`
    /// when the guest has no event buffer posted or the device has no
    /// guest memory bound.
    ///
    /// Until the guest has handled the event, [`Self::poll_rx_injection`]
    /// injects nothing.
    pub fn post_transport_reset(&mut self, qcfg: &QueueConfig) -> bool {
        let Some(ctx) = self.ctx.as_ref() else {
            return false;
        };
        if !qcfg.ready || qcfg.size == 0 {
            return false;
        }
        let event_idx = (self.acked_features & arcbox_virtio_core::queue::VIRTIO_F_EVENT_IDX) != 0;
        let mut queue = SplitQueue::new(ctx.mem.clone(), 2, qcfg, event_idx);
        // The event queue completes every chain it pops, so the cursor is
        // the guest-visible used.idx.
        queue.set_last_avail_idx(queue.used_idx());
        let Some(chain) = queue.pop_avail() else {
            return false;
        };
        let written = chain
            .descriptors
            .iter()
            .find(|d| d.is_write() && d.len as usize >= EVENT_SIZE)
            // SAFETY: a write-only descriptor of a popped chain is owned by
            // the device until the chain is published below.
            .and_then(|d| unsafe { queue.mem().slice_mut(d.addr as usize, EVENT_SIZE) })
            .map(|out| out.copy_from_slice(&EVENT_TRANSPORT_RESET.to_le_bytes()))
            .is_some();
        // Hand an unusable buffer back empty rather than leak it.
        queue.push_used(chain.head_idx, if written { EVENT_SIZE as u32 } else { 0 });
        if written {
            let avail_idx = qcfg.avail_addr as usize + 2;
            self.pending_reset = Some(PendingReset {
                avail_idx,
                posted_at: ctx.mem.read_u16(avail_idx),
            });
        }
        written
    }

    /// Whether no posted TRANSPORT_RESET is still waiting for the guest.
    pub(super) fn reset_handled(&mut self) -> bool {
        let (Some(pending), Some(ctx)) = (&self.pending_reset, &self.ctx) else {
            return true;
        };
        if ctx.mem.read_u16(pending.avail_idx) == pending.posted_at {
            return false;
        }
        self.pending_reset = None;
        true
    }
}
