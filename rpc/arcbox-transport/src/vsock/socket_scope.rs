//! Interrupt socket I/O without extending the connection lifetime.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// Shared cancellation for dedicated guest sockets, including split transports.
///
/// Closing a scope interrupts socket I/O. Callers must still join their workers.
#[derive(Clone, Default)]
pub struct SocketScope {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    closed: bool,
    sockets: Vec<Weak<OwnedFd>>,
}

impl SocketScope {
    /// Rejects new connection attempts after the scope closes.
    pub fn check_open(&self) -> io::Result<()> {
        let state = self.lock()?;
        Self::check_state(&state)
    }

    /// Shuts down every live socket and rejects subsequent registration.
    ///
    /// Attempts every shutdown and returns the first unexpected error.
    /// An already-disconnected socket requires no further shutdown.
    pub fn close(&self) -> io::Result<()> {
        let mut state = self.lock()?;
        state.closed = true;
        let mut result = Ok(());
        for socket in state.sockets.iter().filter_map(Weak::upgrade) {
            // SAFETY: the lease owns a valid duplicate descriptor for this call.
            if unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENOTCONN) {
                    if result.is_ok() {
                        result = Err(error);
                    } else {
                        tracing::error!(%error, "Additional socket scope shutdown failure");
                    }
                }
            }
        }
        result
    }

    pub(super) fn register(&self, socket: BorrowedFd<'_>) -> io::Result<Arc<OwnedFd>> {
        let mut state = self.lock()?;
        Self::check_state(&state)?;
        let lease = Arc::new(socket.try_clone_to_owned()?);
        state.sockets.retain(|socket| socket.strong_count() != 0);
        state.sockets.push(Arc::downgrade(&lease));
        Ok(lease)
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("socket scope lock poisoned"))
    }

    fn check_state(state: &State) -> io::Result<()> {
        if state.closed {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "socket scope is closed",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
