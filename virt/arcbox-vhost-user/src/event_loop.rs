//! The kqueue that drives a back-end's data plane.
//!
//! One kqueue per connection watches every vring kick descriptor, one
//! `EVFILT_USER` trigger per queue (so the control thread can ask the worker
//! to look at a ring it just started or enabled), a stop trigger, and
//! whatever descriptors the device registers through [`Registrar`]. kevent
//! registrations are thread-safe, which lets the control thread add and remove
//! kick descriptors while the worker waits.
//!
//! The event `udata` carries a tagged token: the top two bits say who the
//! event is for, the rest is the queue index or the device's own token.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const TAG_SHIFT: u32 = 62;
const TAG_DEVICE: u64 = 0;
const TAG_KICK: u64 = 1;
const TAG_TRIGGER: u64 = 2;
const TAG_STOP: u64 = 3;
const TOKEN_MASK: u64 = (1 << TAG_SHIFT) - 1;

/// `EVFILT_USER` ident of the stop trigger; queue triggers use the queue
/// index, device wakers count up from [`FIRST_WAKER_IDENT`].
const STOP_IDENT: usize = 0x1_0000;
const FIRST_WAKER_IDENT: usize = 0x1_0001;

/// Largest token a device may register (the top two bits are reserved).
pub const MAX_DEVICE_TOKEN: u64 = TOKEN_MASK;

/// What a kqueue event is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The guest kicked this queue.
    Kick(u16),
    /// The control thread asked the worker to look at this queue.
    Trigger(u16),
    /// The back-end is shutting down.
    Stop,
    /// A descriptor or waker the device registered.
    Device(u64),
}

impl Event {
    const fn encode(self) -> u64 {
        match self {
            Self::Kick(q) => (TAG_KICK << TAG_SHIFT) | q as u64,
            Self::Trigger(q) => (TAG_TRIGGER << TAG_SHIFT) | q as u64,
            Self::Stop => TAG_STOP << TAG_SHIFT,
            Self::Device(token) => (TAG_DEVICE << TAG_SHIFT) | (token & TOKEN_MASK),
        }
    }

    const fn decode(udata: u64) -> Self {
        let value = udata & TOKEN_MASK;
        match udata >> TAG_SHIFT {
            TAG_KICK => Self::Kick(value as u16),
            TAG_TRIGGER => Self::Trigger(value as u16),
            TAG_STOP => Self::Stop,
            _ => Self::Device(value),
        }
    }
}

/// How a device wants a descriptor watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Report the descriptor for as long as it is ready (the default for
    /// handlers that drain to `EAGAIN`).
    Level,
    /// Report it once per readiness change (`EV_CLEAR`); use when a handler
    /// may stop before draining, e.g. because the guest has no RX buffers.
    Edge,
}

/// A kqueue descriptor.
#[derive(Debug)]
pub struct Kqueue {
    fd: OwnedFd,
    next_waker: AtomicUsize,
}

impl Kqueue {
    pub(crate) fn new() -> std::io::Result<Self> {
        // SAFETY: kqueue takes no arguments.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nobody else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: F_SETFD on an owned descriptor.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Self {
            fd,
            next_waker: AtomicUsize::new(FIRST_WAKER_IDENT),
        })
    }

    fn change(
        &self,
        ident: usize,
        filter: i16,
        flags: u16,
        fflags: u32,
        udata: u64,
    ) -> std::io::Result<()> {
        let change = libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: udata as usize as *mut libc::c_void,
        };
        loop {
            // SAFETY: one valid change record, no event list.
            let rc = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    &raw const change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if rc >= 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    pub(crate) fn add_read(
        &self,
        fd: RawFd,
        event: Event,
        trigger: Trigger,
    ) -> std::io::Result<()> {
        let clear = if trigger == Trigger::Edge {
            libc::EV_CLEAR
        } else {
            0
        };
        self.change(
            fd as usize,
            libc::EVFILT_READ,
            libc::EV_ADD | clear,
            0,
            event.encode(),
        )
    }

    pub(crate) fn add_write(
        &self,
        fd: RawFd,
        event: Event,
        trigger: Trigger,
    ) -> std::io::Result<()> {
        let clear = if trigger == Trigger::Edge {
            libc::EV_CLEAR
        } else {
            0
        };
        self.change(
            fd as usize,
            libc::EVFILT_WRITE,
            libc::EV_ADD | clear,
            0,
            event.encode(),
        )
    }

    /// Removes the read and write registrations of `fd`; absent ones are fine.
    pub(crate) fn delete(&self, fd: RawFd) -> std::io::Result<()> {
        for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
            match self.change(fd as usize, filter, libc::EV_DELETE, 0, 0) {
                Err(e) if e.raw_os_error() != Some(libc::ENOENT) => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn add_user(&self, ident: usize, event: Event) -> std::io::Result<()> {
        self.change(
            ident,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            event.encode(),
        )
    }

    /// Fires an `EVFILT_USER` event. XNU stores `udata` on every change, not
    /// just on `EV_ADD`, so the trigger repeats the event it was added with.
    pub(crate) fn trigger_user(&self, ident: usize, event: Event) -> std::io::Result<()> {
        self.change(
            ident,
            libc::EVFILT_USER,
            0,
            libc::NOTE_TRIGGER,
            event.encode(),
        )
    }

    pub(crate) fn trigger_queue(&self, queue: u16) -> std::io::Result<()> {
        self.trigger_user(usize::from(queue), Event::Trigger(queue))
    }

    pub(crate) fn add_stop(&self) -> std::io::Result<()> {
        self.add_user(STOP_IDENT, Event::Stop)
    }

    pub(crate) fn stop(&self) -> std::io::Result<()> {
        self.trigger_user(STOP_IDENT, Event::Stop)
    }

    /// Waits for events and appends what they are for to `out`.
    pub(crate) fn wait(
        &self,
        buf: &mut [libc::kevent],
        out: &mut Vec<Event>,
    ) -> std::io::Result<()> {
        // SAFETY: `buf` is valid for `buf.len()` records; no change list.
        let n = unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                libc::c_int::try_from(buf.len()).unwrap_or(libc::c_int::MAX),
                std::ptr::null(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(err)
            };
        }
        out.extend(
            buf[..n as usize]
                .iter()
                .map(|ev| Event::decode(ev.udata as usize as u64)),
        );
        Ok(())
    }
}

/// Lets a device add its own descriptors and wakers to the back-end's event
/// loop. Cheap to clone, usable from any thread.
#[derive(Debug, Clone)]
pub struct Registrar {
    kq: Arc<Kqueue>,
}

impl Registrar {
    pub(crate) const fn new(kq: Arc<Kqueue>) -> Self {
        Self { kq }
    }

    /// Delivers `token` to [`crate::VhostUserDevice::handle_event`] whenever
    /// `fd` is readable. The device keeps owning `fd` and must call
    /// [`Self::unwatch`] (or close it) when it is done.
    pub fn watch_readable(
        &self,
        fd: BorrowedFd<'_>,
        token: u64,
        trigger: Trigger,
    ) -> std::io::Result<()> {
        check_token(token)?;
        self.kq
            .add_read(fd.as_raw_fd(), Event::Device(token), trigger)
    }

    /// Delivers `token` whenever `fd` is writable.
    pub fn watch_writable(
        &self,
        fd: BorrowedFd<'_>,
        token: u64,
        trigger: Trigger,
    ) -> std::io::Result<()> {
        check_token(token)?;
        self.kq
            .add_write(fd.as_raw_fd(), Event::Device(token), trigger)
    }

    /// Stops watching `fd`. Closing a descriptor also drops its registrations.
    pub fn unwatch(&self, fd: BorrowedFd<'_>) -> std::io::Result<()> {
        self.kq.delete(fd.as_raw_fd())
    }

    /// A handle any thread can use to deliver `token` to the worker, e.g. a
    /// producer that queued host→guest data. Wakes coalesce until handled.
    pub fn waker(&self, token: u64) -> std::io::Result<Waker> {
        check_token(token)?;
        let ident = self.kq.next_waker.fetch_add(1, Ordering::Relaxed);
        self.kq.add_user(ident, Event::Device(token))?;
        Ok(Waker {
            kq: Arc::clone(&self.kq),
            ident,
            token,
        })
    }
}

/// Wakes the back-end's worker with a device token (see [`Registrar::waker`]).
#[derive(Debug, Clone)]
pub struct Waker {
    kq: Arc<Kqueue>,
    ident: usize,
    token: u64,
}

impl Waker {
    /// Delivers the token. Safe to call from any thread, any number of times.
    pub fn wake(&self) -> std::io::Result<()> {
        self.kq.trigger_user(self.ident, Event::Device(self.token))
    }
}

fn check_token(token: u64) -> std::io::Result<()> {
    if token > MAX_DEVICE_TOKEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("device token {token:#x} exceeds {MAX_DEVICE_TOKEN:#x}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    use super::*;

    fn wait(kq: &Kqueue) -> Vec<Event> {
        let mut buf = [libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        }; 8];
        let mut out = Vec::new();
        kq.wait(&mut buf, &mut out).unwrap();
        out
    }

    #[test]
    fn tokens_round_trip_through_udata() {
        for event in [
            Event::Kick(0),
            Event::Kick(u16::MAX),
            Event::Trigger(7),
            Event::Stop,
            Event::Device(0),
            Event::Device(MAX_DEVICE_TOKEN),
        ] {
            assert_eq!(Event::decode(event.encode()), event);
        }
    }

    #[test]
    fn device_descriptors_and_wakers_deliver_their_tokens() {
        let kq = Arc::new(Kqueue::new().unwrap());
        let registrar = Registrar::new(Arc::clone(&kq));
        let (mut a, b) = UnixStream::pair().unwrap();
        registrar
            .watch_readable(std::os::fd::AsFd::as_fd(&b), 42, Trigger::Level)
            .unwrap();
        a.write_all(b"x").unwrap();
        assert_eq!(wait(&kq), vec![Event::Device(42)]);

        registrar.unwatch(std::os::fd::AsFd::as_fd(&b)).unwrap();
        let waker = registrar.waker(7).unwrap();
        let from_thread = waker.clone();
        std::thread::spawn(move || from_thread.wake().unwrap())
            .join()
            .unwrap();
        waker.wake().unwrap();
        // Two wakes before the worker looked coalesce into one event, and the
        // unwatched socket (still readable) reports nothing.
        assert_eq!(wait(&kq), vec![Event::Device(7)]);

        assert!(registrar.waker(MAX_DEVICE_TOKEN + 1).is_err());
    }
}
