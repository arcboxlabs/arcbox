//! The kqueue that drives a back-end's data plane.
//!
//! One kqueue per connection holds one `EVFILT_USER` trigger per queue (so
//! the control thread can ask the worker to look at a ring it just started or
//! enabled) and a stop trigger.
//!
//! The event `udata` carries a tagged token: the top two bits say who the
//! event is for, the rest is the queue index.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const TAG_SHIFT: u32 = 62;
const TAG_TRIGGER: u64 = 2;
const TAG_STOP: u64 = 3;
const TOKEN_MASK: u64 = (1 << TAG_SHIFT) - 1;

/// `EVFILT_USER` ident of the stop trigger; queue triggers use the queue
/// index.
const STOP_IDENT: usize = 0x1_0000;

/// What a kqueue event is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The control thread asked the worker to look at this queue.
    Trigger(u16),
    /// The back-end is shutting down.
    Stop,
}

impl Event {
    const fn encode(self) -> u64 {
        match self {
            Self::Trigger(q) => (TAG_TRIGGER << TAG_SHIFT) | q as u64,
            Self::Stop => TAG_STOP << TAG_SHIFT,
        }
    }

    const fn decode(udata: u64) -> Self {
        let value = udata & TOKEN_MASK;
        match udata >> TAG_SHIFT {
            TAG_TRIGGER => Self::Trigger(value as u16),
            _ => Self::Stop,
        }
    }
}

/// A kqueue descriptor.
#[derive(Debug)]
pub struct Kqueue {
    fd: OwnedFd,
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
        Ok(Self { fd })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_through_udata() {
        for event in [Event::Trigger(7), Event::Stop] {
            assert_eq!(Event::decode(event.encode()), event);
        }
    }
}
