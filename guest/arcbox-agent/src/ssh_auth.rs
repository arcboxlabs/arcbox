//! SSH agent forwarding into containers (guest side).
//!
//! A container opts into agent forwarding exactly as it would on OrbStack or
//! Docker Desktop:
//!
//! ```text
//! docker run \
//!   -v /run/host-services/ssh-auth.sock:/run/host-services/ssh-auth.sock \
//!   -e SSH_AUTH_SOCK=/run/host-services/ssh-auth.sock …
//! ```
//!
//! This agent binds that Unix socket inside the System VM and relays every
//! connection to the daemon, which in turn connects to the user's real
//! `ssh-agent` on the Mac. The wrinkle is direction: the resource
//! (`ssh-agent`) lives on the *host*, but the connection originates in the
//! *guest*, and the VZ backend exposes only host→guest dialing — the guest can
//! never dial the host. So the daemon pre-dials a pool of connections to
//! [`SSH_AUTH_RELAY_PORT`] and *parks* them; this module hands each parked
//! connection to the next container that opens the forwarded socket.
//!
//! The parked connection is also how the "host must never stop reading a vsock
//! stream" rule of the VZ backend is honoured: while parked, the daemon sits
//! reading and this side sends nothing, so neither direction stalls. When a
//! container pairs, this side writes a single [`PAIR_MARKER`] byte
//! (guest→host, which the daemon is already positioned to read); the daemon
//! takes that as the signal to connect to the Mac `ssh-agent`, then both sides
//! splice. If the agent is absent the daemon closes the slot, the container
//! sees EOF, and `ssh-add` fails cleanly instead of hanging.

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

use arcbox_constants::paths::HOST_SERVICES_SSH_AUTH_SOCK;
use arcbox_constants::ports::SSH_AUTH_RELAY_PORT;

/// One byte the guest writes into a parked slot to wake the daemon. Its value
/// is irrelevant — the daemon reads and drops exactly one byte — but a
/// well-known value keeps a packet capture legible. The daemon writes nothing
/// on a parked slot, so this is always the first byte it reads there.
const PAIR_MARKER: u8 = 0x01;

/// How long a container waits for a free relay slot before it gets a clean
/// error instead of hanging. `ssh-agent` operations are quick, so a backed-up
/// pool clears fast; this only bounds pathological saturation or a daemon that
/// has stopped re-parking (e.g. it exited).
const PAIR_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs the SSH-auth forwarding relay until `cancel` fires.
///
/// Binds the container-facing Unix socket and the vsock slot listener, then
/// pairs each container connection with a healthy parked slot from the daemon.
pub async fn run_ssh_auth_relay(cancel: CancellationToken) {
    let vsock_listener = match VsockListener::bind(VsockAddr::new(
        VMADDR_CID_ANY,
        SSH_AUTH_RELAY_PORT,
    )) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(port = SSH_AUTH_RELAY_PORT, error = %e, "failed to bind ssh-auth vsock relay");
            return;
        }
    };

    let unix_listener = match bind_unix_socket() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(path = HOST_SERVICES_SSH_AUTH_SOCK, error = %e, "failed to bind ssh-auth unix socket");
            return;
        }
    };

    tracing::info!(
        vsock_port = SSH_AUTH_RELAY_PORT,
        path = HOST_SERVICES_SSH_AUTH_SOCK,
        "ssh-auth relay listening"
    );

    // Parked slots flow from the vsock acceptor to the coordinator below.
    // Unbounded so accepting a freshly-dialled slot never blocks the acceptor.
    let (slot_tx, mut slot_rx) = mpsc::unbounded_channel::<VsockStream>();

    let acceptor = tokio::spawn(accept_parked_slots(vsock_listener, slot_tx, cancel.clone()));

    // Coordinator: pair each container connection with a healthy parked slot.
    // Slot acquisition is inline (single consumer of `slot_rx`, no lock), the
    // splice is spawned. Waiting for a slot briefly delays the next container
    // under >pool saturation, which for a dev pool of quick operations is
    // acceptable — a freed slot arrives on the same channel we are awaiting.
    loop {
        let (unix_stream, _) = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            result = unix_listener.accept() => match result {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "ssh-auth unix accept failed");
                    continue;
                }
            }
        };

        match acquire_slot(&mut slot_rx, &cancel).await {
            Some(slot) => {
                tokio::spawn(pump(unix_stream, slot));
            }
            None => {
                // No slot within the timeout: either the daemon is unreachable
                // or the pool is saturated. Dropping the connection gives the
                // container a clean EOF rather than an indefinite hang.
                tracing::warn!("ssh-auth: no relay slot available; is the ArcBox daemon running?");
                drop(unix_stream);
            }
        }
    }

    let _ = acceptor.await;
}

/// Accepts host-dialled connections and forwards each as a parked slot.
async fn accept_parked_slots(
    mut listener: VsockListener,
    slot_tx: mpsc::UnboundedSender<VsockStream>,
    cancel: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            result = listener.accept() => match result {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "ssh-auth vsock accept failed");
                    continue;
                }
            }
        };
        if slot_tx.send(stream).is_err() {
            return;
        }
    }
}

/// Pulls parked slots until one probes healthy, discarding dead ones, or gives
/// up after [`PAIR_TIMEOUT`] or shutdown.
///
/// The daemon may have restarted since a slot was parked, leaving stale
/// connections queued ahead of live ones; [`slot_is_parked`] tells them apart.
async fn acquire_slot(
    slot_rx: &mut mpsc::UnboundedReceiver<VsockStream>,
    cancel: &CancellationToken,
) -> Option<VsockStream> {
    let deadline = tokio::time::Instant::now() + PAIR_TIMEOUT;
    loop {
        let slot = tokio::select! {
            biased;
            () = cancel.cancelled() => return None,
            () = tokio::time::sleep_until(deadline) => return None,
            slot = slot_rx.recv() => slot?,
        };
        if slot_is_parked(&slot) {
            return Some(slot);
        }
        tracing::debug!("ssh-auth: discarded a stale parked slot");
    }
}

/// Writes the pairing marker into the slot, then splices the container's
/// `ssh-agent` traffic through it.
async fn pump(mut unix: UnixStream, mut slot: VsockStream) {
    if let Err(e) = slot.write_all(&[PAIR_MARKER]).await {
        tracing::debug!(error = %e, "ssh-auth: failed to signal a parked slot");
        return;
    }
    if let Err(e) = copy_bidirectional(&mut unix, &mut slot).await {
        tracing::debug!(error = %e, "ssh-auth: relay copy ended with error");
    }
}

/// True when a parked slot is still live: a non-destructive peek finds neither
/// pending data nor EOF.
///
/// The daemon parks by reading and writes nothing until it sees the marker, so
/// a live slot has nothing to peek (`EAGAIN`). A slot from a since-exited
/// daemon peeks EOF (`0`) or an error; either way it is not a fresh slot.
fn slot_is_parked(slot: &VsockStream) -> bool {
    fd_has_no_pending_input(slot.as_raw_fd())
}

fn fd_has_no_pending_input(fd: RawFd) -> bool {
    let mut byte = [0u8; 1];
    // SAFETY: `fd` is a valid open socket owned by the caller's stream. MSG_PEEK
    // is non-destructive and MSG_DONTWAIT keeps the call from blocking.
    let n = unsafe {
        libc::recv(
            fd,
            byte.as_mut_ptr().cast::<libc::c_void>(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        // EAGAIN == EWOULDBLOCK on Linux: alive with nothing pending.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN)
    } else {
        // 0 = EOF (daemon gone); >0 = unexpected data before a marker.
        false
    }
}

/// Binds the container-facing forwarding socket at
/// [`HOST_SERVICES_SSH_AUTH_SOCK`], replacing any stale socket a previous agent
/// left behind.
///
/// The socket is made world-accessible: containers connect through the
/// bind-mount as arbitrary uids, and the System VM it lives in is
/// single-tenant, so `ssh-agent` reachability, not filesystem permissions, is
/// the access boundary here.
fn bind_unix_socket() -> std::io::Result<UnixListener> {
    let path = Path::new(HOST_SERVICES_SSH_AUTH_SOCK);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::fd_has_no_pending_input;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn parked_slot_with_no_pending_data_reads_as_live() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        // Nothing written by the peer: a live, parked slot.
        assert!(fd_has_no_pending_input(a.as_raw_fd()));
    }

    #[test]
    fn slot_with_pending_data_is_not_a_fresh_slot() {
        let (a, mut b) = UnixStream::pair().expect("socketpair");
        b.write_all(b"x").expect("write");
        // Peer sent something the daemon never would before a marker.
        assert!(!fd_has_no_pending_input(a.as_raw_fd()));
    }

    #[test]
    fn closed_peer_reads_as_dead() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        drop(b);
        // EOF: the far end (a since-exited daemon) is gone.
        assert!(!fd_has_no_pending_input(a.as_raw_fd()));
    }
}
