//! Flow control on a session's connection (`MachineExecWindow` in
//! agent.proto).
//!
//! The host sets `output_window` in its request: the session then sends
//! output frames only within the window the host holds open — counted in
//! encoded frame bytes, the final frame excepted — and grants the host a
//! stdin window (in stdin bytes) of its own. Neither side sends what the
//! other has no room for, so both can always keep reading the connection:
//! a reader that stops draining a vsock connection stalls every connection
//! to the VM.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::bail;
use tokio::sync::Semaphore;

use super::session::OUTPUT_CHUNK;
use crate::rpc::ErrorResponse;

/// Smallest output window that holds a full output frame: a chunk plus its
/// encoding.
const MIN_OUTPUT_WINDOW: usize = 2 * OUTPUT_CHUNK;

/// Stdin the session holds for its process: the window it grants the host.
pub(super) const STDIN_WINDOW: u32 = 256 * 1024;

pub(super) struct Flow {
    /// Output window the host holds open; `None` without flow control.
    output: Option<Semaphore>,
    output_limit: usize,
    /// Stdin received from the host and not yet delivered to the process.
    stdin_queued: AtomicUsize,
}

impl Flow {
    /// The flow control a request asks for (`output_window == 0`: none).
    pub(super) fn new(output_window: u32) -> Result<Self, ErrorResponse> {
        let output_limit = output_window as usize;
        if output_limit != 0 && output_limit < MIN_OUTPUT_WINDOW {
            return Err(ErrorResponse::new(
                400,
                format!("output_window must be 0 or at least {MIN_OUTPUT_WINDOW} bytes"),
            ));
        }
        Ok(Self {
            output: (output_limit != 0).then(|| Semaphore::new(output_limit)),
            output_limit,
            stdin_queued: AtomicUsize::new(0),
        })
    }

    /// The stdin window to grant the host before any other frame.
    pub(super) fn initial_stdin_window(&self) -> Option<u32> {
        self.output.as_ref().map(|_| STDIN_WINDOW)
    }

    /// Waits until the host has room for an output frame of `len` encoded
    /// bytes, and takes that room. Cancel-safe: nothing is taken unless it
    /// completes.
    pub(super) async fn reserve_output(&self, len: usize) {
        if let Some(window) = &self.output {
            // The semaphore is never closed.
            if let Ok(permit) = window.acquire_many(len as u32).await {
                permit.forget();
            }
        }
    }

    /// Output window the host returned as its consumer took output.
    pub(super) fn return_output(&self, bytes: u32) -> anyhow::Result<()> {
        let Some(window) = &self.output else {
            bail!("output window returned to a session without flow control");
        };
        if window.available_permits() + bytes as usize > self.output_limit {
            bail!("host returned more output window than it was given");
        }
        window.add_permits(bytes as usize);
        Ok(())
    }

    /// Accounts for stdin the host sent; with flow control on, sending
    /// more than the window granted is a protocol error.
    pub(super) fn admit_stdin(&self, len: usize) -> anyhow::Result<()> {
        let queued = self.stdin_queued.fetch_add(len, Ordering::AcqRel) + len;
        if self.output.is_some() && queued > STDIN_WINDOW as usize {
            bail!("host sent {queued} bytes of stdin into a {STDIN_WINDOW}-byte window");
        }
        Ok(())
    }

    /// Accounts for stdin delivered to the process (or dropped because it
    /// closed its stdin); returns the window to give back to the host.
    pub(super) fn stdin_delivered(&self, len: usize) -> Option<u32> {
        self.stdin_queued.fetch_sub(len, Ordering::AcqRel);
        self.output.as_ref().map(|_| len as u32)
    }
}
