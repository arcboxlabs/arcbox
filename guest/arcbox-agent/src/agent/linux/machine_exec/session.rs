//! What a running session does to its process and connection besides
//! streaming: the exit frame, host-requested signals, and killing the
//! session when the host goes away.

use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use arcbox_connect::v1::MachineExecOutput;
use buffa::Message as _;
use nix::sys::signal::Signal;
use tokio::io::AsyncWrite;

use crate::rpc::{MessageType, write_message};

/// Writes the final frame reporting how the process ended.
pub(super) async fn write_exit<W>(
    writer: &mut W,
    trace_id: &str,
    status: ExitStatus,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let out = MachineExecOutput {
        done: true,
        exit_code: status.code().unwrap_or(-1),
        exit_signal: status.signal().map(signal_name).unwrap_or_default(),
        ..Default::default()
    };
    write_message(
        writer,
        MessageType::MachineExecOutput,
        trace_id,
        &out.encode_to_vec(),
    )
    .await
}

/// A signal's name without the `SIG` prefix, as SSH reports it (`"KILL"`).
fn signal_name(signal: i32) -> String {
    Signal::try_from(signal).map_or_else(
        |_| signal.to_string(),
        |s| s.as_str().trim_start_matches("SIG").to_owned(),
    )
}

/// Delivers a host-requested signal to the session's process, as sshd does
/// for an SSH `signal` request (the process itself, not its group).
pub(super) fn signal_process(pid: Option<u32>, signal: Signal) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        if let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            tracing::debug!(error = %e, ?signal, "machine exec signal not delivered");
        }
    }
}

/// Kills a session's whole process group: the child leads its own session,
/// so descendants must not outlive a host that went away.
pub(super) fn kill_session(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
    }
}
