//! Answering the FUSE request carried by one descriptor chain.

use arcbox_virtio_core::{DescChain, GuestMemWriter};

use super::VirtioFs;
use crate::protocol::MAX_REQUEST_SIZE;
use crate::request::FuseResponse;

impl VirtioFs {
    /// Answers the FUSE request in `chain`'s readable descriptors and returns
    /// the reply for its writable ones.
    ///
    /// Descriptor lengths are the guest's: up to a queue's worth of them,
    /// each up to 4 GiB and free to cover the same range, so their total is
    /// checked before anything is copied. A chain longer than
    /// [`MAX_REQUEST_SIZE`] is answered `EINVAL`.
    pub fn answer_chain(&mut self, mem: &GuestMemWriter, chain: &DescChain) -> Vec<u8> {
        let len: u64 = chain
            .descriptors
            .iter()
            .filter(|d| !d.is_write())
            .map(|d| u64::from(d.len))
            .sum();
        if len > MAX_REQUEST_SIZE as u64 {
            tracing::warn!(len, "virtio-fs request longer than any FUSE request");
            return FuseResponse::error(request_unique(&gather(mem, chain, 16)), libc::EINVAL)
                .into_data();
        }
        let request = gather(mem, chain, MAX_REQUEST_SIZE);
        self.process_request(&request).unwrap_or_else(|e| {
            tracing::warn!("virtio-fs request failed: {e}");
            FuseResponse::error(request_unique(&request), libc::EIO).into_data()
        })
    }
}

/// Copies at most `limit` bytes from the chain's readable descriptors.
fn gather(mem: &GuestMemWriter, chain: &DescChain, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for d in chain.descriptors.iter().filter(|d| !d.is_write()) {
        let room = limit - out.len();
        if room == 0 {
            break;
        }
        if let Some(bytes) = mem.slice(d.addr as usize, (d.len as usize).min(room)) {
            out.extend_from_slice(bytes);
        }
    }
    out
}

/// The `unique` of a FUSE request, or 0 when its header is cut short.
pub(super) fn request_unique(request: &[u8]) -> u64 {
    request
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}
