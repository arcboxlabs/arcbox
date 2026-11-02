//! Per-queue state.
//!
//! A [`Vring`] collects what the front-end configured (`SET_VRING_NUM`,
//! `_ADDR`, `_KICK`, `_ENABLE`) and, once the ring is started, owns the
//! [`SplitQueue`] that walks it.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use arcbox_virtio_core::{QueueConfig, SplitQueue};

use crate::error::{Error, Result};
use crate::memory::GuestMemory;
use crate::protocol::VringAddr;

/// One virtqueue as the front-end configured it.
pub struct Vring {
    index: u16,
    max_size: u16,
    size: u16,
    addr: Option<VringAddr>,
    base: u16,
    kick: Option<OwnedFd>,
    started: bool,
    enabled: bool,
    event_idx: bool,
    memory: Option<Arc<GuestMemory>>,
    queue: Option<SplitQueue>,
}

impl Vring {
    pub(crate) fn new(index: u16, max_size: u16) -> Self {
        Self {
            index,
            max_size,
            size: 0,
            addr: None,
            base: 0,
            kick: None,
            started: false,
            enabled: false,
            event_idx: false,
            memory: None,
            queue: None,
        }
    }

    /// Queue index within the device.
    #[must_use]
    pub const fn index(&self) -> u16 {
        self.index
    }

    /// Whether the ring is started and enabled, i.e. the device may consume
    /// and publish buffers on it.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.queue.is_some() && self.enabled
    }

    // ---- control plane (front-end messages) -------------------------------

    pub(crate) fn set_size(&mut self, size: u32) -> Result<()> {
        if size == 0 || size > u32::from(self.max_size) || !size.is_power_of_two() {
            return Err(Error::Protocol(format!(
                "queue {} size {size} (supported: power of two up to {})",
                self.index, self.max_size
            )));
        }
        self.size = size as u16;
        Ok(())
    }

    pub(crate) fn set_addr(&mut self, addr: VringAddr) -> Result<()> {
        self.addr = Some(addr);
        self.rebuild()
    }

    /// Installs a kick descriptor and starts the ring. Returns the previous
    /// kick descriptor.
    pub(crate) fn start(&mut self, kick: Option<OwnedFd>) -> Result<Option<OwnedFd>> {
        if let Some(fd) = &kick {
            set_nonblocking(fd)?;
        }
        let old = std::mem::replace(&mut self.kick, kick);
        self.started = true;
        self.rebuild()?;
        Ok(old)
    }

    pub(crate) const fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn set_event_idx(&mut self, event_idx: bool) {
        self.event_idx = event_idx;
        if let Some(queue) = self.queue.as_mut() {
            queue.set_event_idx(event_idx);
        }
    }

    pub(crate) fn set_memory(&mut self, memory: Arc<GuestMemory>) -> Result<()> {
        self.memory = Some(memory);
        self.rebuild()
    }

    /// Back to the state right after connection setup.
    pub(crate) fn reset(&mut self) -> Option<OwnedFd> {
        let kick = self.kick.take();
        *self = Self::new(self.index, self.max_size);
        kick
    }

    /// (Re)builds the queue of a started ring from the current memory, size
    /// and addresses, keeping the avail cursor. A ring that is not started, or
    /// still lacks memory or addresses, has no queue.
    fn rebuild(&mut self) -> Result<()> {
        if let Some(queue) = self.queue.take() {
            self.base = queue.last_avail_idx();
        }
        if !self.started {
            return Ok(());
        }
        let (Some(memory), Some(addr)) = (&self.memory, &self.addr) else {
            return Ok(());
        };
        if self.size == 0 {
            return Err(Error::Protocol(format!(
                "queue {} started without a size",
                self.index
            )));
        }
        let (desc, avail, used) = translate(memory, addr, self.size)?;
        let config = QueueConfig {
            desc_addr: desc,
            avail_addr: avail,
            used_addr: used,
            size: self.size,
            ready: true,
            gpa_base: memory.gpa_base(),
        };
        let mut queue = SplitQueue::new(memory.writer(), self.index, &config, self.event_idx);
        queue.set_last_avail_idx(self.base);
        self.queue = Some(queue);
        Ok(())
    }
}

/// Puts a descriptor into non-blocking mode.
fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL on a descriptor we own only read/change its
    // status flags.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

impl std::fmt::Debug for Vring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vring")
            .field("index", &self.index)
            .field("size", &self.size)
            .field("started", &self.started)
            .field("enabled", &self.enabled)
            .field("event_idx", &self.event_idx)
            .field(
                "last_avail_idx",
                &self
                    .queue
                    .as_ref()
                    .map_or(self.base, SplitQueue::last_avail_idx),
            )
            .finish_non_exhaustive()
    }
}

/// Translates the front-end's ring addresses into guest physical addresses,
/// checking that each ring lies wholly inside one memory region.
fn translate(memory: &GuestMemory, addr: &VringAddr, size: u16) -> Result<(u64, u64, u64)> {
    let size = u64::from(size);
    // Split-ring layout (virtio 1.x §2.7): 16-byte descriptors; avail is
    // flags, idx, ring[size], used_event; used is flags, idx, ring[size] of
    // 8 bytes, avail_event.
    let rings = [
        (addr.desc_user_addr, 16 * size, "descriptor table"),
        (addr.avail_user_addr, 6 + 2 * size, "available ring"),
        (addr.used_user_addr, 6 + 8 * size, "used ring"),
    ];
    let mut gpas = [0u64; 3];
    for (gpa, (uva, len, what)) in gpas.iter_mut().zip(rings) {
        *gpa = memory.uva_to_gpa(uva, len).ok_or_else(|| {
            Error::Protocol(format!(
                "queue {} {what} at front-end address {uva:#x} (+{len}) is outside guest memory",
                addr.index
            ))
        })?;
    }
    Ok(gpas.into())
}
