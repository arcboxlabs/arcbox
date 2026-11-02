//! A stand-in vhost-user-fs device for the `arcbox` share.
//!
//! Serves it the way `virt/arcbox-vhost-user/examples/qemu_fs_probe.rs`
//! does: the HV backend's `VirtioFs` session and `FsServer` fed from
//! vhost-user vrings, until the QEMU backend's own fs device lands.

use std::path::Path;
use std::sync::Arc;

use arcbox_fs::{CacheProfile, FsServer};
use arcbox_vhost_user::{VhostUserDevice, Vrings};
use arcbox_virtio_core::{DescChain, GuestMemWriter, VirtioDevice};
use arcbox_virtio_fs::{FsConfig, FuseResponse, VirtioFs};

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;

/// The largest request copied out of the guest. FUSE_INIT settles on a
/// `max_write` of 128 KiB, so anything past 1 MiB is a broken guest, refused
/// before its bytes are copied.
const MAX_REQUEST_BYTES: usize = 1 << 20;

pub struct FsDevice {
    fs: VirtioFs,
}

impl FsDevice {
    pub fn new(tag: &str, root: &Path) -> std::io::Result<Self> {
        let source = root.to_string_lossy().into_owned();
        let mut server = FsServer::new(arcbox_fs::FsConfig {
            tag: tag.into(),
            source: source.clone(),
            cache_profile: CacheProfile::Dynamic,
            ..arcbox_fs::FsConfig::default()
        });
        server.start().map_err(std::io::Error::other)?;
        let fs = VirtioFs::with_handler(
            FsConfig {
                tag: tag.into(),
                num_queues: 1,
                queue_size: 1024,
                shared_dir: source,
            },
            Arc::new(server),
        );
        Ok(Self { fs })
    }
}

impl VhostUserDevice for FsDevice {
    fn num_queues(&self) -> usize {
        2 // hiprio + one request queue
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_RING_F_EVENT_IDX
    }

    fn process_queue(&mut self, queue: u16, vrings: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        if let Some(vring) = vrings.get_mut(queue) {
            let fs = &mut self.fs;
            vring.process_avail(|mem, chain| handle_chain(fs, mem, chain));
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.fs.reset();
    }
}

fn handle_chain(fs: &mut VirtioFs, mem: &GuestMemWriter, chain: &DescChain) -> u32 {
    let mut readable = chain.descriptors.iter().filter(|d| !d.is_write());
    let size: usize = readable.clone().map(|d| d.len as usize).sum();
    let reply = if size > MAX_REQUEST_BYTES {
        // Only the header is read, for the `unique` the error reply carries.
        let head = readable
            .next()
            .and_then(|d| mem.slice(d.addr as usize, (d.len as usize).min(16)));
        tracing::warn!("FUSE request of {size} bytes refused");
        FuseResponse::error(unique(head.unwrap_or_default()), libc::E2BIG).into_data()
    } else {
        let mut request = Vec::with_capacity(size);
        for d in readable {
            if let Some(bytes) = mem.slice(d.addr as usize, d.len as usize) {
                request.extend_from_slice(bytes);
            }
        }
        fs.process_request(&request).unwrap_or_else(|e| {
            tracing::warn!("FUSE request failed: {e}");
            FuseResponse::error(unique(&request), libc::EIO).into_data()
        })
    };
    let mut written = 0;
    for d in chain.descriptors.iter().filter(|d| d.is_write()) {
        let rest = &reply[written..];
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(d.len as usize);
        // SAFETY: writable descriptors belong to the device until the used
        // ring publishes the chain.
        if let Some(dst) = unsafe { mem.slice_mut(d.addr as usize, n) } {
            dst.copy_from_slice(&rest[..n]);
            written += n;
        }
    }
    written as u32
}

/// The `unique` of a FUSE request header, or 0 when it is too short.
fn unique(request: &[u8]) -> u64 {
    request
        .get(8..16)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}
