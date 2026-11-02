//! virtio-fs over vhost-user.
//!
//! One back-end per shared directory, answering FUSE with the same
//! [`FsServer`] the HV backend runs, so a guest sees the same shares,
//! handler and cache timeouts whichever backend booted it.
//!
//! The front-end owns the tag and the queue layout:
//!
//! ```text
//! -chardev socket,id=fs0,fd=<theirs>
//! -device vhost-user-fs-pci,chardev=fs0,tag=<FsShare::tag>,queue-size=1024,num-request-queues=1
//! ```
//!
//! There is no DAX window (no shared-memory region on the device), so every
//! read is a `FUSE_READ`; the agent's `dax=always` remount of `/arcbox`
//! succeeds because virtiofs ignores options on remount. `FUSE_SYNCFS` is
//! answered `ENOSYS`, as on HV; the guest kernel then stops sending it and
//! `sync` succeeds.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcbox_fs::FsServer;
use arcbox_virtio_core::{DescChain, GuestMemWriter, VirtioDevice};
use arcbox_virtio_fs::VirtioFs;

use crate::{Backend, BackendHandle, Error, Result, VhostUserDevice, Vrings};

/// Request queues per share. The front-end's `num-request-queues` must match.
pub const REQUEST_QUEUES: usize = 1;

/// Largest `queue-size` the front-end may configure.
pub const QUEUE_SIZE: u16 = 1024;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;

/// A host directory shared with the guest under a virtio-fs tag.
///
/// Matches what the HV backend builds per `SharedDirConfig`: the tag and the
/// host path, with `arcbox_fs::FsConfig` defaults for everything else. Like
/// HV, there is no read-only mode and no uid mapping; FUSE requests run with
/// the daemon's credentials.
#[derive(Debug, Clone)]
pub struct FsShare {
    /// Guest mount tag (the front-end's `tag=`; the back-end never sends it).
    pub tag: String,
    /// Host directory served as the share's root.
    pub source: PathBuf,
}

impl FsShare {
    /// A share of `source` under `tag`.
    pub fn new(tag: impl Into<String>, source: impl Into<PathBuf>) -> Self {
        Self {
            tag: tag.into(),
            source: source.into(),
        }
    }

    /// Serves the share on `stream`, the back-end end of the socketpair whose
    /// other end the front-end's `-chardev socket,fd=` holds. Start it before
    /// or right after QEMU: QEMU blocks in device realize until it answers.
    ///
    /// # Errors
    /// Fails when the source directory cannot be opened or the back-end
    /// thread cannot start.
    pub fn spawn(self, stream: UnixStream) -> Result<FsBackend> {
        let device = FsDevice {
            fs: self.session()?,
            stats: FsStats::default(),
        };
        let handle = Backend::new(format!("vhost-fs-{}", self.tag), device).spawn(stream)?;
        Ok(FsBackend {
            tag: self.tag,
            handle,
        })
    }

    /// The FUSE session over a started `FsServer`, configured as HV's
    /// `darwin_hv` setup configures it.
    fn session(&self) -> Result<VirtioFs> {
        let source = self.source.to_string_lossy().into_owned();
        let mut server = FsServer::new(arcbox_fs::FsConfig {
            tag: self.tag.clone(),
            source: source.clone(),
            ..arcbox_fs::FsConfig::default()
        });
        server
            .start()
            .map_err(|e| Error::Device(format!("virtio-fs share {}: {e}", self.tag)))?;
        Ok(VirtioFs::with_handler(
            arcbox_virtio_fs::FsConfig {
                tag: self.tag.clone(),
                num_queues: REQUEST_QUEUES as u32,
                queue_size: QUEUE_SIZE,
                shared_dir: source,
            },
            Arc::new(server),
        ))
    }
}

/// A share being served; ends when the front-end disconnects or on
/// [`Self::stop`].
pub struct FsBackend {
    tag: String,
    handle: BackendHandle<FsDevice>,
}

impl FsBackend {
    /// The share's tag.
    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Disconnects from the front-end and waits for the back-end to finish.
    ///
    /// # Errors
    /// Returns the connection's failure, if it ended on one.
    pub fn stop(self) -> Result<FsStats> {
        self.handle.stop().map(|device| device.stats)
    }

    /// Waits for the front-end to disconnect.
    ///
    /// # Errors
    /// Returns the connection's failure, if it ended on one.
    pub fn join(self) -> Result<FsStats> {
        self.handle.join().map(|device| device.stats)
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

/// What a share's back-end did over its connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsStats {
    /// FUSE requests answered, both queues.
    pub requests: u64,
    /// Time spent answering them.
    pub busy: Duration,
    /// Most requests taken in one queue notification: how far the guest got
    /// ahead of the back-end.
    pub max_batch: usize,
}

struct FsDevice {
    fs: VirtioFs,
    stats: FsStats,
}

impl VhostUserDevice for FsDevice {
    fn num_queues(&self) -> usize {
        // Queue 0 is hiprio (FORGET, INTERRUPT); the rest carry requests.
        1 + REQUEST_QUEUES
    }

    fn max_queue_size(&self) -> u16 {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_RING_F_EVENT_IDX
    }

    fn process_queue(&mut self, queue: u16, vrings: &mut Vrings) -> Result<()> {
        let Some(vring) = vrings.get_mut(queue) else {
            return Ok(());
        };
        let started = Instant::now();
        let fs = &mut self.fs;
        let handled = vring.process_avail(|mem, chain| handle_chain(fs, mem, chain));
        if handled > 0 {
            self.stats.requests += handled as u64;
            self.stats.busy += started.elapsed();
            self.stats.max_batch = self.stats.max_batch.max(handled);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.fs.reset();
    }
}

/// Answers the request in the chain's readable descriptors (a chain longer
/// than any FUSE request gets `EINVAL`, see [`VirtioFs::answer_chain`]) and
/// scatters the reply into the writable ones. Returns the bytes written.
fn handle_chain(fs: &mut VirtioFs, mem: &GuestMemWriter, chain: &DescChain) -> u32 {
    let reply = fs.answer_chain(mem, chain);
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

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_virtio_core::VirtqDesc;
    use arcbox_virtio_fs::MAX_REQUEST_SIZE;

    const WRITE: u16 = 2;

    fn desc(addr: u64, len: u32, flags: u16) -> VirtqDesc {
        VirtqDesc {
            addr,
            len,
            flags,
            next: 0,
        }
    }

    /// FUSE_INIT (opcode 26, FUSE 7.31): a 40-byte header and a 16-byte body.
    fn fuse_init(unique: u64) -> Vec<u8> {
        let mut init = Vec::new();
        for v in [56u32, 26] {
            init.extend_from_slice(&v.to_le_bytes());
        }
        init.extend_from_slice(&unique.to_le_bytes());
        init.resize(40, 0);
        for v in [7u32, 31, 128 << 10, 0] {
            init.extend_from_slice(&v.to_le_bytes());
        }
        init
    }

    /// The guest splits requests and replies across descriptors wherever its
    /// buffers fall; both directions must cross descriptor boundaries intact.
    #[test]
    fn requests_and_replies_span_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let mut fs = FsShare::new("t", dir.path()).session().unwrap();

        // INIT with unique 7, header and body in separate readable descriptors.
        let mut ram = vec![0u8; 4096];
        ram[..56].copy_from_slice(&fuse_init(7));
        // SAFETY: `ram` outlives the writer.
        let mem = unsafe { GuestMemWriter::new(ram.as_mut_ptr(), ram.len(), 0) };
        let chain = DescChain {
            head_idx: 0,
            descriptors: vec![
                desc(0, 40, 0),
                desc(40, 16, 0),
                desc(1024, 30, WRITE),
                desc(2048, 100, WRITE),
            ],
        };

        assert_eq!(handle_chain(&mut fs, &mem, &chain), 80);
        drop(mem);
        let reply = [&ram[1024..1054], &ram[2048..2098]].concat();
        assert_eq!(reply[0..4], 80u32.to_le_bytes(), "out header length");
        assert_eq!(reply[4..8], 0i32.to_le_bytes(), "no error");
        assert_eq!(reply[8..16], 7u64.to_le_bytes(), "unique echoed");
        assert_eq!(reply[16..20], 7u32.to_le_bytes(), "FUSE major");
        assert!(
            ram[2098..2148].iter().all(|&b| b == 0),
            "nothing past the reply"
        );
    }

    /// A chain longer than any FUSE request is refused before it is copied,
    /// since every descriptor may repeat one large range of guest memory.
    #[test]
    fn requests_longer_than_a_full_write_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut fs = FsShare::new("t", dir.path()).session().unwrap();
        fs.process_request(&fuse_init(1)).unwrap();

        // FUSE_GETATTR (opcode 3, unique 9) of the root, padded to `len`
        // bytes by a second readable descriptor.
        let reply_at = 0x3_0000;
        let mut ram = vec![0u8; 0x4_0000];
        let mut getattr = |len: usize| -> i32 {
            ram[0..4].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
            ram[4..8].copy_from_slice(&3u32.to_le_bytes());
            ram[8..16].copy_from_slice(&9u64.to_le_bytes());
            ram[16..24].copy_from_slice(&1u64.to_le_bytes());
            ram[reply_at..].fill(0);
            // SAFETY: `ram` outlives the writer.
            let mem = unsafe { GuestMemWriter::new(ram.as_mut_ptr(), ram.len(), 0) };
            let padding = u32::try_from(len - 56).unwrap();
            let chain = DescChain {
                head_idx: 0,
                descriptors: vec![
                    desc(0, 56, 0),
                    desc(4096, padding, 0),
                    desc(reply_at as u64, 256, WRITE),
                ],
            };
            handle_chain(&mut fs, &mem, &chain);
            drop(mem);
            let reply = &ram[reply_at..];
            assert_eq!(reply[8..16], 9u64.to_le_bytes(), "unique echoed");
            i32::from_le_bytes(reply[4..8].try_into().unwrap())
        };

        assert_eq!(
            getattr(MAX_REQUEST_SIZE),
            0,
            "a full-size request is served"
        );
        assert_eq!(getattr(MAX_REQUEST_SIZE + 1), -libc::EINVAL);
    }
}
