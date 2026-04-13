//! Bridge NIC (NIC2) — vmnet bridge for container IP routing.
//!
//! This is the second virtio-net device in the HV backend. Its peer socket
//! is connected to vmnet (Apple's host network interface framework) so the
//! guest's container IP subnet can reach the macOS host's default route.
//!
//! The TX path (`handle_bridge_tx`) extracts frames from the guest's TX
//! virtqueue and writes them to the vmnet relay fd. The RX path
//! (`poll_bridge_rx`) drains inbound frames from the vmnet fd and injects
//! them into the guest's RX virtqueue.

use arcbox_virtio::QueueConfig;
use arcbox_virtio::net::VirtioNetHeader;

use super::{DeviceManager, checksum::finalize_virtio_net_checksum};

impl DeviceManager {
    /// Extracts ethernet frames from bridge VirtioNet TX queue and writes
    /// them to the bridge (vmnet) host fd. Mirrors `handle_net_tx`.
    pub(super) fn handle_bridge_tx(&self, memory: &[u8], qcfg: &QueueConfig) -> Vec<(u16, u32)> {
        let Some(bridge_fd) = self.bridge_host_fd else {
            return Vec::new();
        };
        if !qcfg.ready || qcfg.size == 0 {
            return Vec::new();
        }

        // Translate GPAs to slice offsets (checked against ram base).
        let gpa_base = qcfg.gpa_base as usize;
        let Some(desc_addr) = (qcfg.desc_addr as usize).checked_sub(gpa_base) else {
            tracing::warn!(
                "handle_bridge_tx: desc GPA {:#x} below ram base {:#x}",
                qcfg.desc_addr,
                gpa_base
            );
            return Vec::new();
        };
        let Some(avail_addr) = (qcfg.avail_addr as usize).checked_sub(gpa_base) else {
            tracing::warn!(
                "handle_bridge_tx: avail GPA {:#x} below ram base {:#x}",
                qcfg.avail_addr,
                gpa_base
            );
            return Vec::new();
        };
        let q_size = qcfg.size as usize;

        if avail_addr + 4 > memory.len() {
            return Vec::new();
        }
        let avail_idx = u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]);

        let mut current_avail = self
            .bridge_last_avail_tx
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut completions = Vec::new();

        while current_avail != avail_idx {
            let ring_off = avail_addr + 4 + 2 * ((current_avail as usize) % q_size);
            if ring_off + 2 > memory.len() {
                break;
            }
            let head_idx = u16::from_le_bytes([memory[ring_off], memory[ring_off + 1]]) as usize;

            let mut packet_data = Vec::new();
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > memory.len() {
                    break;
                }
                let addr = match (u64::from_le_bytes(memory[d_off..d_off + 8].try_into().unwrap())
                    as usize)
                    .checked_sub(gpa_base)
                {
                    Some(a) => a,
                    None => continue,
                };
                let len =
                    u32::from_le_bytes(memory[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(memory[d_off + 12..d_off + 14].try_into().unwrap());
                let next = u16::from_le_bytes(memory[d_off + 14..d_off + 16].try_into().unwrap());

                if flags & 2 == 0 && addr + len <= memory.len() {
                    packet_data.extend_from_slice(&memory[addr..addr + len]);
                }
                if flags & 1 == 0 {
                    break;
                }
                idx = next as usize;
            }

            let total_len = packet_data.len() as u32;
            finalize_virtio_net_checksum(&mut packet_data);

            // Strip the virtio-net header after applying checksum offload.
            if packet_data.len() > VirtioNetHeader::SIZE {
                let frame = &packet_data[VirtioNetHeader::SIZE..];
                // SAFETY: `bridge_fd` is owned by the DeviceManager and live
                // for the call. `frame` is a valid byte slice borrowed from
                // `packet_data` for the duration of the write.
                let n = unsafe {
                    libc::write(
                        bridge_fd,
                        frame.as_ptr().cast::<libc::c_void>(),
                        frame.len(),
                    )
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EAGAIN) {
                        tracing::warn!("bridge TX write failed: {err}");
                    }
                }
            }

            completions.push((head_idx as u16, total_len));
            current_avail = current_avail.wrapping_add(1);
        }

        self.bridge_last_avail_tx
            .store(current_avail, std::sync::atomic::Ordering::Relaxed);
        completions
    }

    /// Polls the bridge (vmnet) host fd for incoming frames and injects them
    /// into the bridge VirtioNet RX queue. Mirrors `poll_net_rx`.
    pub fn poll_bridge_rx(&self) -> bool {
        let Some(bridge_fd) = self.bridge_host_fd else {
            return false;
        };
        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return false;
        };
        let gpa_base = self.guest_ram_gpa as usize;
        // SAFETY: `ram_base` is the host mapping returned by
        // Virtualization.framework and is valid for `ram_size` bytes.
        let guest_mem = unsafe { std::slice::from_raw_parts_mut(ram_base, ram_size) };

        // Find the bridge VirtioNet device's MMIO state.
        let bridge_device_id = match self.bridge_net_device_id {
            Some(id) => id,
            None => return false,
        };
        let device = match self.devices.get(&bridge_device_id) {
            Some(d) => d,
            None => return false,
        };
        let mmio_arc = match device.mmio_state.as_ref() {
            Some(m) => m,
            None => return false,
        };
        let Ok(mmio) = mmio_arc.read() else {
            return false;
        };

        let qi = 0usize; // RX queue
        if qi >= 8 || !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
            return false;
        }

        // Translate GPAs to slice offsets (checked against ram base).
        let Some(desc_addr) = (mmio.queue_desc[qi] as usize).checked_sub(gpa_base) else {
            return false;
        };
        let Some(avail_addr) = (mmio.queue_driver[qi] as usize).checked_sub(gpa_base) else {
            return false;
        };
        let Some(used_addr) = (mmio.queue_device[qi] as usize).checked_sub(gpa_base) else {
            return false;
        };
        let q_size = mmio.queue_num[qi] as usize;
        drop(mmio);

        if avail_addr + 4 > guest_mem.len() {
            return false;
        }

        let mut injected = false;

        // Read up to 64 frames per poll (same as NIC1).
        let used_idx_off = used_addr + 2;
        let mut used_idx =
            u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]);

        for _ in 0..64 {
            // Re-read avail_idx each iteration so newly posted buffers are
            // picked up without waiting for the next poll cycle.
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let avail_idx =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]);

            if avail_idx == used_idx {
                break; // No RX descriptors available.
            }

            // Non-blocking read from bridge fd.
            let mut buf = [0u8; 9216]; // MAX_FRAME_SIZE
            // SAFETY: `bridge_fd` is owned by the DeviceManager and live
            // for the call. `buf` is a valid mutable stack slice of
            // `buf.len()` bytes. MSG_DONTWAIT ensures non-blocking.
            let n = unsafe {
                libc::recv(
                    bridge_fd,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            let frame = &buf[..n as usize];

            // Prepend 12-byte virtio-net header (all zeros = no offload).
            let virtio_hdr = [0u8; 12];
            let total = virtio_hdr.len() + frame.len();

            // Pop an available RX descriptor and write header + frame.
            let ring_off = avail_addr + 4 + 2 * ((used_idx as usize) % q_size);
            if ring_off + 2 > guest_mem.len() {
                break;
            }
            let head_idx =
                u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

            let mut written = 0;
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > guest_mem.len() {
                    break;
                }
                let addr_gpa =
                    u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
                let len = u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap())
                    as usize;
                let flags =
                    u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
                let next =
                    u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());
                let Some(addr) = addr_gpa.checked_sub(gpa_base) else {
                    continue;
                };

                if flags & 2 != 0 && addr + len <= guest_mem.len() {
                    // Write from [virtio_hdr | frame] combined.
                    let remaining = total.saturating_sub(written);
                    let to_write = remaining.min(len);
                    if to_write > 0 {
                        // Scatter from virtio_hdr then frame.
                        let hdr_remaining = virtio_hdr.len().saturating_sub(written);
                        if hdr_remaining > 0 {
                            let hdr_write = hdr_remaining.min(to_write);
                            guest_mem[addr..addr + hdr_write]
                                .copy_from_slice(&virtio_hdr[written..written + hdr_write]);
                            if to_write > hdr_write {
                                let frame_write = to_write - hdr_write;
                                guest_mem[addr + hdr_write..addr + hdr_write + frame_write]
                                    .copy_from_slice(&frame[..frame_write]);
                            }
                        } else {
                            let frame_off = written - virtio_hdr.len();
                            guest_mem[addr..addr + to_write]
                                .copy_from_slice(&frame[frame_off..frame_off + to_write]);
                        }
                        written += to_write;
                    }
                }

                if flags & 1 == 0 || written >= total {
                    break;
                }
                idx = next as usize;
            }

            if written == 0 {
                continue;
            }

            // Update used ring.
            let used_entry = used_addr + 4 + ((used_idx as usize) % q_size) * 8;
            if used_entry + 8 <= guest_mem.len() {
                guest_mem[used_entry..used_entry + 4]
                    .copy_from_slice(&(head_idx as u32).to_le_bytes());
                guest_mem[used_entry + 4..used_entry + 8]
                    .copy_from_slice(&(written as u32).to_le_bytes());
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                used_idx = used_idx.wrapping_add(1);
                guest_mem[used_idx_off..used_idx_off + 2].copy_from_slice(&used_idx.to_le_bytes());
            }

            injected = true;
        }

        // Write avail_event for EVENT_IDX.
        if injected {
            let avail_idx_now =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]);
            let avail_event_off = used_addr + 4 + q_size * 8;
            if avail_event_off + 2 <= guest_mem.len() {
                guest_mem[avail_event_off..avail_event_off + 2]
                    .copy_from_slice(&avail_idx_now.to_le_bytes());
            }
        }

        injected
    }
}
