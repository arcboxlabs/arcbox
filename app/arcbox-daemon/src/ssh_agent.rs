//! Host side of SSH agent forwarding into containers.
//!
//! A container opts in the OrbStack / Docker-Desktop way:
//!
//! ```text
//! docker run \
//!   -v /run/host-services/ssh-auth.sock:/run/host-services/ssh-auth.sock \
//!   -e SSH_AUTH_SOCK=/run/host-services/ssh-auth.sock …
//! ```
//!
//! The guest agent binds that Unix socket and relays each connection to this
//! daemon, which connects to the user's real `ssh-agent` on the Mac. The
//! direction is the hard part: the resource lives on the host but the
//! connection starts in the guest, and the VZ backend dials only host→guest.
//!
//! So this module runs a small pool of workers, each of which pre-dials one
//! vsock connection to [`SSH_AUTH_RELAY_PORT`] and *parks* it — sits reading,
//! which is also what the VZ "host must never stop reading a vsock stream"
//! rule requires. When a container opens the forwarded socket, the guest
//! writes one marker byte into a parked connection; the worker reads it, then
//! connects to the Mac `ssh-agent` and splices the two. If no agent can be
//! found the worker just closes the connection, so the container sees EOF and
//! `ssh-add` fails cleanly instead of hanging. After each forward the worker
//! re-parks, so the pool self-heals across container churn and VM restarts.
//!
//! Set `ARCBOX_SSH_AGENT_FORWARDING=0` to disable the feature entirely.

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arcbox_constants::ports::SSH_AUTH_RELAY_PORT;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime, VmLifecycleState};
use arcbox_transport::vsock::{VsockShutdown, VsockStream};
use tokio::io::{AsyncReadExt, copy_bidirectional};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;

/// Number of connections kept parked in the guest at once. Each is a slot a
/// container can be handed the instant it opens the forwarded socket, so this
/// is how many *first-touch* forwards are served without waiting for a worker
/// to re-park. A worker re-parks as soon as its forward ends, so steady-state
/// throughput is not capped at this — eight simply covers a handful of
/// containers doing git/ssh at the same moment.
const POOL_SIZE: usize = 8;

/// Backoff before a worker retries after a failed park (transient connect
/// error, or the VM stopped while parked). Readiness is awaited separately, so
/// this only paces genuine errors, never a cleanly-stopped VM.
const REDIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Spawns the SSH-auth forwarding worker pool unless disabled by env.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    if !forwarding_enabled() {
        info!("ssh-auth agent forwarding disabled via ARCBOX_SSH_AGENT_FORWARDING");
        return;
    }

    for id in 0..POOL_SIZE {
        let runtime = Arc::clone(runtime);
        let shutdown = ctx.shutdown.clone();
        tokio::spawn(run_worker(id, runtime, shutdown));
    }
    info!(pool = POOL_SIZE, "ssh-auth agent forwarding enabled");
}

/// True unless `ARCBOX_SSH_AGENT_FORWARDING` is set to a falsy value.
fn forwarding_enabled() -> bool {
    match std::env::var("ARCBOX_SSH_AGENT_FORWARDING") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// One pool worker: keep a connection parked in the guest whenever the VM is
/// ready, serve each container that pairs with it, and re-park.
async fn run_worker(id: usize, runtime: Arc<Runtime>, shutdown: CancellationToken) {
    let mut state = runtime.subscribe_system_vm_state();
    loop {
        // Zero-poll wait for a ready VM: dialing a stopped one only returns a
        // "not running" error, so park attempts are gated on the ready edge.
        if !wait_until_ready(&mut state, &shutdown).await {
            return;
        }

        match serve_once(&runtime, &shutdown).await {
            Ok(()) => {}
            Err(e) => {
                debug!(worker = id, error = %e, "ssh-auth: park attempt failed; retrying");
                if !sleep_unless_cancelled(REDIAL_BACKOFF, &shutdown).await {
                    return;
                }
            }
        }
    }
}

/// Parks one connection and, once a container pairs with it, forwards to the
/// Mac `ssh-agent`.
///
/// Returns `Ok` after a forward completes or after a missing agent is handled
/// (the connection is dropped, giving the container a clean error). Returns
/// `Err` only when parking itself failed — a transient connect error, or the
/// VM stopping while parked — which the worker paces before retrying.
async fn serve_once(runtime: &Arc<Runtime>, shutdown: &CancellationToken) -> Result<()> {
    let mut slot = dial_slot(runtime).await?;

    // Park: block reading the one-byte pairing marker. This is the posture VZ
    // requires (the host stays reading), and the wait ends only when a
    // container opens the forwarded socket, the VM stops, or we shut down.
    let mut marker = [0u8; 1];
    let read = tokio::select! {
        biased;
        () = shutdown.cancelled() => return Ok(()),
        r = slot.read_exact(&mut marker) => r,
    };
    read.context("parked slot closed before a container paired")?;

    // A container paired. Connect to the user's agent lazily, now that there is
    // demand — resolving it up front would race the user starting their agent.
    let Some(sock_path) = resolve_ssh_auth_sock() else {
        warn!(
            "ssh-auth: no ssh-agent found on the Mac (SSH_AUTH_SOCK unset); \
             closing the forward so the container reports a clear error"
        );
        return Ok(());
    };

    let mut agent = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %sock_path.display(), error = %e, "ssh-auth: cannot reach the Mac ssh-agent");
            return Ok(());
        }
    };

    tokio::select! {
        biased;
        () = shutdown.cancelled() => {}
        r = copy_bidirectional(&mut slot, &mut agent) => {
            if let Err(e) = r {
                debug!(error = %e, "ssh-auth: forward copy ended with error");
            }
        }
    }
    Ok(())
}

/// Dials one parked slot to [`SSH_AUTH_RELAY_PORT`] on the guest.
///
/// `connect_vsock_port` is a blocking hypervisor call on both backends, so it
/// runs off the async executor.
async fn dial_slot(runtime: &Arc<Runtime>) -> Result<VsockStream> {
    let rt = Arc::clone(runtime);
    let fd = tokio::task::spawn_blocking(move || {
        rt.connect_vsock_port(DEFAULT_MACHINE_NAME, SSH_AUTH_RELAY_PORT)
    })
    .await??;

    // SAFETY: `fd` is a valid, newly-opened vsock fd handed over by the
    // hypervisor layer; ownership transfers to the OwnedFd here.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let stream = VsockStream::from_fd_with_shutdown(owned, VsockShutdown::CloseOnDropOnly)?;
    Ok(stream)
}

/// Resolves the user's `ssh-agent` socket path.
///
/// A launchd-started daemon usually has no `SSH_AUTH_SOCK` in its own
/// environment, so `launchctl getenv` — which returns the per-user value — is
/// the load-bearing fallback. `ARCBOX_SSH_AUTH_SOCK` lets a test or an unusual
/// setup pin the path explicitly.
fn resolve_ssh_auth_sock() -> Option<PathBuf> {
    for var in ["ARCBOX_SSH_AUTH_SOCK", "SSH_AUTH_SOCK"] {
        if let Some(value) = std::env::var_os(var) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    launchctl_getenv("SSH_AUTH_SOCK")
}

/// Reads a variable out of the per-user launchd environment.
fn launchctl_getenv(name: &str) -> Option<PathBuf> {
    let output = Command::new("/bin/launchctl")
        .arg("getenv")
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "(null)" {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

/// Waits until the System VM is ready, returning `false` if the daemon shut
/// down or the lifecycle channel was dropped first.
async fn wait_until_ready(
    state: &mut watch::Receiver<VmLifecycleState>,
    shutdown: &CancellationToken,
) -> bool {
    loop {
        if shutdown.is_cancelled() {
            return false;
        }
        if state.borrow_and_update().is_ready() {
            return true;
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return false,
            changed = state.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

/// Sleeps for `duration`, returning `false` if the daemon shut down instead.
async fn sleep_unless_cancelled(duration: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        () = tokio::time::sleep(duration) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::forwarding_enabled;

    /// The env gate is the documented off switch, so its truth table is worth
    /// pinning: falsy spellings disable, everything else (including unset)
    /// leaves the feature on.
    #[test]
    fn env_gate_defaults_on_and_honors_falsy_values() {
        // SAFETY: single-threaded test; no other thread reads the environment.
        let restore = std::env::var_os("ARCBOX_SSH_AGENT_FORWARDING");

        unsafe { std::env::remove_var("ARCBOX_SSH_AGENT_FORWARDING") };
        assert!(forwarding_enabled(), "unset means enabled");

        for falsy in ["0", "false", "off", "no", "OFF", " false "] {
            unsafe { std::env::set_var("ARCBOX_SSH_AGENT_FORWARDING", falsy) };
            assert!(!forwarding_enabled(), "{falsy:?} should disable");
        }

        for truthy in ["1", "true", "on", "yes", "anything"] {
            unsafe { std::env::set_var("ARCBOX_SSH_AGENT_FORWARDING", truthy) };
            assert!(forwarding_enabled(), "{truthy:?} should enable");
        }

        match restore {
            Some(v) => unsafe { std::env::set_var("ARCBOX_SSH_AGENT_FORWARDING", v) },
            None => unsafe { std::env::remove_var("ARCBOX_SSH_AGENT_FORWARDING") },
        }
    }
}
