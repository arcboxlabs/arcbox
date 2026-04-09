//! VirtIO RX queue configuration and descriptor chain operations.

use std::sync::atomic::Ordering;

use crate::guest_mem::GuestMemWriter;

/// Virtio-net header size (all zeros for RX passthrough).
pub const VIRTIO_NET_HDR_SIZE: usize = 12;

/// Immutable RX queue layout, captured once at DRIVER_OK time.
/// All GPAs are absolute guest physical addresses, translated to
/// host offsets by `GuestMemWriter::gpa_to_offset`.
pub struct RxQueueConfig {
    pub desc_gpa: u64,
    pub avail_gpa: u64,
    pub used_gpa: u64,
    pub size: u16,
}

/// Injects a single Ethernet frame into the guest RX queue.
///
/// Prepends a 12-byte zeroed virtio-net header, walks the descriptor
/// chain (scatter-gather), and updates the used ring.
///
/// Returns `true` if the frame was successfully injected, `false`
/// if no RX descriptors are available or the frame couldn't be written.
pub fn inject_one_frame(
    guest_mem: &GuestMemWriter,
    queue: &RxQueueConfig,
    frame: &[u8],
    used_idx: &mut u16,
) -> bool {
    let q_size = queue.size as usize;
    if q_size == 0 {
        return false;
    }

    // Read avail_idx from guest memory.
    std::sync::atomic::fence(Ordering::Acquire);
    let avail_idx = guest_mem.read_u16(queue.avail_gpa as usize + 2);
    if *used_idx == avail_idx {
        return false; // No RX descriptors available.
    }

    // Pop available descriptor.
    let ring_off = queue.avail_gpa as usize + 4 + 2 * ((*used_idx as usize) % q_size);
    let head_idx = guest_mem.read_u16(ring_off) as usize;

    // Build packet: virtio-net header (zeroed) + ethernet frame.
    let total_len = VIRTIO_NET_HDR_SIZE + frame.len();

    // Walk descriptor chain and write the packet.
    let mut written = 0;
    let mut idx = head_idx;
    let desc_base = queue.desc_gpa as usize;

    for _ in 0..q_size {
        let d_off = desc_base + idx * 16;
        let Some(desc_slice) = guest_mem.slice(d_off, 16) else {
            break;
        };

        let addr_gpa = u64::from_le_bytes(desc_slice[0..8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(desc_slice[8..12].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(desc_slice[12..14].try_into().unwrap());
        let next = u16::from_le_bytes(desc_slice[14..16].try_into().unwrap());

        // RX descriptors are device-writable (flag bit 1).
        if flags & 2 != 0 && len > 0 {
            // SAFETY: VirtIO descriptor buffers are device-owned during
            // injection. The guest will not access them until used_idx
            // advances (after Release fence below).
            let Some(buf) = (unsafe { guest_mem.slice_mut(addr_gpa, len) }) else {
                continue;
            };

            let remaining = total_len.saturating_sub(written);
            let to_write = remaining.min(len);

            if written < VIRTIO_NET_HDR_SIZE {
                // Write virtio-net header (or partial header).
                let hdr_remaining = VIRTIO_NET_HDR_SIZE - written;
                let hdr_bytes = hdr_remaining.min(to_write);
                buf[..hdr_bytes].fill(0); // Zeroed virtio-net header.
                // Write frame data after header.
                let frame_bytes = to_write - hdr_bytes;
                if frame_bytes > 0 {
                    buf[hdr_bytes..hdr_bytes + frame_bytes].copy_from_slice(&frame[..frame_bytes]);
                }
            } else {
                // Pure frame data.
                let frame_off = written - VIRTIO_NET_HDR_SIZE;
                buf[..to_write].copy_from_slice(&frame[frame_off..frame_off + to_write]);
            }
            written += to_write;
        }

        if flags & 1 == 0 || written >= total_len {
            break;
        }
        idx = next as usize;
    }

    if written == 0 {
        return false;
    }

    // Update used ring entry.
    let used_entry_off = queue.used_gpa as usize + 4 + ((*used_idx as usize) % q_size) * 8;
    guest_mem.write_u32(used_entry_off, head_idx as u32);
    guest_mem.write_u32(used_entry_off + 4, written as u32);

    // Release fence: ensure descriptor data is visible before used_idx.
    std::sync::atomic::fence(Ordering::Release);
    *used_idx = used_idx.wrapping_add(1);
    guest_mem.write_u16(queue.used_gpa as usize + 2, *used_idx);

    true
}
