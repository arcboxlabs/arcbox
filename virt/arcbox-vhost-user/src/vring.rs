//! Per-queue state and the device-facing queue API.
//!
//! A [`Vring`] collects what the front-end configured (`SET_VRING_NUM`,
//! `_ADDR`, `_BASE`, `_KICK`, `_CALL`, `_ENABLE`) and, once the ring is
//! started, owns the [`SplitQueue`] the device processes it through. The
//! `SplitQueue` is the single owner of the avail cursor: `GET_VRING_BASE`
//! reports it, `SET_VRING_BASE` seeds it.
//!
//! Notifications on macOS are pipes, not eventfds: QEMU hands over the read
//! end of a pipe as the kick descriptor and the write end as the call
//! descriptor (`event_notifier_get_wfd`), writes 8 bytes per kick, and drains
//! whatever the back-end writes to the call pipe. Both are drained or written
//! non-blocking; a full call pipe already means an interrupt is pending.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, PoisonError, RwLock};

use arcbox_virtio_core::{DescChain, GuestMemWriter, QueueConfig, SplitQueue};

use crate::error::{Error, Result};
use crate::memory::GuestMemory;
use crate::protocol::VringAddr;

/// Raises a queue's call notifier, making QEMU interrupt the guest.
///
/// Cheap to clone and usable from any thread. A clone always writes to the
/// descriptor the front-end installed last: QEMU swaps it on every MSI-X
/// vector mask and unmask, and an interrupt written to a replaced descriptor
/// is lost.
#[derive(Debug, Clone, Default)]
pub struct CallNotifier {
    fd: Arc<RwLock<Option<OwnedFd>>>,
}

impl CallNotifier {
    fn replace(&self, fd: Option<OwnedFd>) -> std::io::Result<()> {
        if let Some(fd) = &fd {
            set_nonblocking(fd)?;
        }
        *self.fd.write().unwrap_or_else(PoisonError::into_inner) = fd;
        Ok(())
    }

    /// Asks the front-end to interrupt the guest for this queue; a no-op
    /// while no call descriptor is installed.
    pub fn notify(&self) {
        let guard = self.fd.read().unwrap_or_else(PoisonError::into_inner);
        let Some(fd) = guard.as_ref() else { return };
        let value = 1u64.to_ne_bytes();
        loop {
            // SAFETY: writes 8 bytes from a live buffer to a descriptor we own.
            let n = unsafe { libc::write(fd.as_raw_fd(), value.as_ptr().cast(), value.len()) };
            if n >= 0 {
                return;
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => {}
                // Full pipe: an interrupt is already pending.
                std::io::ErrorKind::WouldBlock => return,
                _ => {
                    tracing::debug!("vhost-user call notifier write failed: {err}");
                    return;
                }
            }
        }
    }
}

/// One virtqueue as the front-end configured it.
pub struct Vring {
    index: u16,
    max_size: u16,
    size: u16,
    addr: Option<VringAddr>,
    base: u16,
    kick: Option<OwnedFd>,
    call: CallNotifier,
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
            call: CallNotifier::default(),
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

    /// The queue, while the ring is running.
    pub fn queue_mut(&mut self) -> Option<&mut SplitQueue> {
        if self.enabled {
            self.queue.as_mut()
        } else {
            None
        }
    }

    /// The ring's guest-physical layout, while it is running — for devices
    /// that drive their own `SplitQueue` (the HV vsock injection path takes a
    /// `QueueConfig`). Such a device owns the avail cursor then, and must
    /// report it back through [`Self::set_last_avail_idx`] before it returns.
    #[must_use]
    pub fn queue_config(&self) -> Option<QueueConfig> {
        let queue = self.queue.as_ref().filter(|_| self.enabled)?;
        let addr = self.addr.as_ref()?;
        let memory = self.memory.as_ref()?;
        let (desc, avail, used) = translate(memory, addr, queue.size()).ok()?;
        Some(QueueConfig {
            desc_addr: desc,
            avail_addr: avail,
            used_addr: used,
            size: queue.size(),
            ready: true,
            gpa_base: memory.gpa_base(),
        })
    }

    /// Seeds the avail cursor of the running queue (see [`Self::queue_config`]).
    pub fn set_last_avail_idx(&mut self, idx: u16) {
        if let Some(queue) = self.queue.as_mut() {
            queue.set_last_avail_idx(idx);
        }
    }

    /// Guest memory, while the ring is running. Stays mapped until the
    /// connection ends; drop it in [`crate::VhostUserDevice::reset`].
    #[must_use]
    pub fn memory(&self) -> Option<Arc<GuestMemWriter>> {
        self.memory
            .as_ref()
            .filter(|_| self.is_running())
            .map(|m| m.writer())
    }

    /// The call notifier, for devices that interrupt from their own code
    /// path. It follows every later `SET_VRING_CALL`, so it may be kept.
    #[must_use]
    pub fn call_notifier(&self) -> CallNotifier {
        self.call.clone()
    }

    /// Interrupts the guest for this queue (no-op without a call descriptor).
    pub fn signal_used(&self) {
        self.call.notify();
    }

    /// Consumes every available chain: `handle` gets the guest memory and the
    /// chain and returns how many bytes it wrote into the chain's writable
    /// descriptors. Publishes each completion, re-arms EVENT_IDX
    /// notifications (draining again if the guest posted more meanwhile), and
    /// interrupts the guest once at the end if it asked for it. Returns the
    /// number of chains consumed.
    pub fn process_avail<F>(&mut self, mut handle: F) -> usize
    where
        F: FnMut(&GuestMemWriter, &DescChain) -> u32,
    {
        let Some(queue) = self.queue_mut() else {
            return 0;
        };
        let mut consumed = 0;
        let mut notify = false;
        loop {
            while let Some(chain) = queue.pop_avail() {
                let written = handle(queue.mem(), &chain);
                notify |= queue.push_used(chain.head_idx, written);
                consumed += 1;
            }
            if !queue.enable_notification() {
                break;
            }
        }
        if notify {
            self.signal_used();
        }
        consumed
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

    pub(crate) fn set_base(&mut self, base: u32) -> Result<()> {
        self.base = u16::try_from(base).map_err(|_| {
            Error::Unsupported(format!(
                "queue {} base {base:#x} (packed rings are not supported)",
                self.index
            ))
        })?;
        if let Some(queue) = self.queue.as_mut() {
            queue.set_last_avail_idx(self.base);
        }
        Ok(())
    }

    /// Installs a kick descriptor and starts the ring. Returns the previous
    /// kick descriptor so the caller can drop its kqueue registration.
    pub(crate) fn start(&mut self, kick: Option<OwnedFd>) -> Result<Option<OwnedFd>> {
        if let Some(fd) = &kick {
            set_nonblocking(fd)?;
        }
        let old = std::mem::replace(&mut self.kick, kick);
        self.started = true;
        self.rebuild()?;
        Ok(old)
    }

    /// Stops the ring (`GET_VRING_BASE`) and returns the avail cursor and the
    /// kick descriptor to unregister.
    pub(crate) fn stop(&mut self) -> (u16, Option<OwnedFd>) {
        if let Some(queue) = self.queue.take() {
            self.base = queue.last_avail_idx();
        }
        self.started = false;
        (self.base, self.kick.take())
    }

    pub(crate) fn set_call(&self, call: Option<OwnedFd>) -> Result<()> {
        self.call.replace(call)?;
        Ok(())
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

    pub(crate) fn kick_fd(&self) -> Option<&OwnedFd> {
        self.kick.as_ref()
    }

    /// Empties the kick pipe. Kicks only say "look at the ring"; their count
    /// carries nothing. Returns `false` once the front-end closed its end, so
    /// the caller stops watching a descriptor that would report EOF forever.
    pub(crate) fn drain_kick(&self) -> bool {
        let Some(fd) = &self.kick else { return true };
        let mut buf = [0u8; 512];
        loop {
            // SAFETY: reads into a live stack buffer from a descriptor we own.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                0 => return false,
                n if n < 0 => {
                    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        return true;
                    }
                }
                n if (n as usize) < buf.len() => return true,
                _ => {}
            }
        }
    }

    /// Back to the state right after connection setup. Notifier clones the
    /// device holds stay attached and fire again once QEMU installs a new
    /// call descriptor.
    pub(crate) fn reset(&mut self) -> Option<OwnedFd> {
        let kick = self.kick.take();
        let call = std::mem::take(&mut self.call);
        let _ = call.replace(None);
        *self = Self {
            call,
            ..Self::new(self.index, self.max_size)
        };
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

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: pipe writes two descriptors into `fds`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: fresh descriptors, sole owner.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn pending(fd: &OwnedFd) -> usize {
        let mut n: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int.
        assert_eq!(
            unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &raw mut n) },
            0
        );
        n as usize
    }

    #[test]
    fn a_kept_notifier_follows_call_descriptor_changes() {
        let mut vring = Vring::new(0, 8);
        let kept = vring.call_notifier();
        kept.notify(); // nothing installed yet: no-op

        let (masked_rd, masked_wr) = pipe();
        vring.set_call(Some(masked_wr)).unwrap();
        kept.notify();
        assert_eq!(pending(&masked_rd), 8);

        // QEMU unmasks the vector: the same clone must now hit the new pipe.
        let (guest_rd, guest_wr) = pipe();
        vring.set_call(Some(guest_wr)).unwrap();
        kept.notify();
        assert_eq!(pending(&guest_rd), 8);
        assert_eq!(pending(&masked_rd), 8);

        // A reset detaches the descriptor but keeps the clone attached.
        let _ = vring.reset();
        kept.notify();
        assert_eq!(pending(&guest_rd), 8);
        let (again_rd, again_wr) = pipe();
        vring.set_call(Some(again_wr)).unwrap();
        kept.notify();
        assert_eq!(pending(&again_rd), 8);
    }
}
