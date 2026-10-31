//! Requests to a running datapath from threads outside its runtime.
//!
//! A machine checkpoint uses both: [`DatapathControl::abort_tcp`] collects
//! a reset for every TCP flow instead of sending it to the stopped guest,
//! and the restored machine's datapath hands those resets to the resumed
//! guest with [`DatapathControl::send_to_guest`]. Without them a guest that
//! waits on a connection, and so sends nothing, never learns that its host
//! side is gone.

use std::io;
use std::sync::mpsc as sync_mpsc;
use std::time::Duration;

use tokio::sync::mpsc;

pub(super) enum Command {
    AbortTcp(sync_mpsc::SyncSender<Vec<Vec<u8>>>),
    SendToGuest(Vec<Vec<u8>>),
}

/// Sends requests to one datapath's event loop. Cheap to clone.
#[derive(Clone)]
pub struct DatapathControl {
    pub(super) tx: mpsc::UnboundedSender<Command>,
}

impl DatapathControl {
    /// Aborts every TCP flow ([`splicetcp::tcp_bridge::TcpBridge::abort`]) and
    /// returns the guest-bound resets instead of sending them.
    ///
    /// The guest must not run, and nothing may write its RX queue, while
    /// the loop handles this: each reset carries the sequence number the
    /// guest has received up to. Blocks for at most `timeout`; do not call
    /// from the datapath's runtime.
    ///
    /// # Errors
    ///
    /// Fails when the loop has exited or does not answer within `timeout`.
    pub fn abort_tcp(&self, timeout: Duration) -> io::Result<Vec<Vec<u8>>> {
        let (reply, answer) = sync_mpsc::sync_channel(1);
        self.tx
            .send(Command::AbortTcp(reply))
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))?;
        answer
            .recv_timeout(timeout)
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))
    }

    /// Queues Ethernet frames for the guest, reliably and in order.
    ///
    /// # Errors
    ///
    /// Fails when the loop has exited.
    pub fn send_to_guest(&self, frames: Vec<Vec<u8>>) -> io::Result<()> {
        self.tx
            .send(Command::SendToGuest(frames))
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }
}
