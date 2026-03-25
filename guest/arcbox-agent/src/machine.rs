//! Machine-level command execution.
//!
//! Handles `MachineRunRequest` by spawning a process directly in the guest VM
//! and streaming stdout/stderr back as `MachineRunOutput` frames over the wire.

use std::process::Stdio;

use anyhow::Context;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::process::Command;

use crate::rpc::{ErrorResponse, MessageType, write_message};

/// Handles a machine-level run request.
///
/// Spawns a process directly in the guest VM and streams stdout/stderr
/// back as `MachineRunOutput` frames.  A final frame with `done = true`
/// carries the exit code.
pub async fn handle_machine_run<S>(
    stream: &mut S,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let req = arcbox_protocol::MachineRunRequest::decode(payload)
        .context("failed to decode MachineRunRequest")?;

    if req.cmd.is_empty() {
        let err = ErrorResponse::new(400, "cmd must not be empty");
        write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
        return Ok(());
    }

    let mut cmd = Command::new(&req.cmd[0]);
    if req.cmd.len() > 1 {
        cmd.args(&req.cmd[1..]);
    }
    if !req.working_dir.is_empty() {
        cmd.current_dir(&req.working_dir);
    }
    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    // Resolve user if specified.
    if !req.user.is_empty() {
        let uid = resolve_uid(&req.user)?;
        // SAFETY: `setuid` is an async-signal-safe POSIX call and `uid` is
        // a valid value obtained from the passwd database above.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = ErrorResponse::new(500, format!("failed to spawn process: {e}"));
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_buf = [0u8; 8192];
    let mut stderr_buf = [0u8; 8192];
    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            res = stdout.read(&mut stdout_buf), if !stdout_done => {
                match res {
                    Ok(0) => stdout_done = true,
                    Ok(n) => {
                        let out = arcbox_protocol::MachineRunOutput {
                            stream: "stdout".into(),
                            data: stdout_buf[..n].to_vec(),
                            ..Default::default()
                        };
                        write_message(stream, MessageType::MachineRunOutput, trace_id, &out.encode_to_vec()).await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stdout read error");
                        stdout_done = true;
                    }
                }
            }
            res = stderr.read(&mut stderr_buf), if !stderr_done => {
                match res {
                    Ok(0) => stderr_done = true,
                    Ok(n) => {
                        let out = arcbox_protocol::MachineRunOutput {
                            stream: "stderr".into(),
                            data: stderr_buf[..n].to_vec(),
                            ..Default::default()
                        };
                        write_message(stream, MessageType::MachineRunOutput, trace_id, &out.encode_to_vec()).await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stderr read error");
                        stderr_done = true;
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("failed to wait for child")?;
    let exit_code = status.code().unwrap_or(-1);

    let final_out = arcbox_protocol::MachineRunOutput {
        done: true,
        exit_code,
        ..Default::default()
    };
    write_message(
        stream,
        MessageType::MachineRunOutput,
        trace_id,
        &final_out.encode_to_vec(),
    )
    .await?;

    Ok(())
}

/// Resolves a username or numeric UID string to a `uid_t`.
fn resolve_uid(user: &str) -> anyhow::Result<libc::uid_t> {
    // Try numeric first.
    if let Ok(uid) = user.parse::<libc::uid_t>() {
        return Ok(uid);
    }
    // Lookup by name via getpwnam.
    let c_name = std::ffi::CString::new(user).context("invalid user name")?;
    // SAFETY: `c_name` is a valid nul-terminated C string and `getpwnam`
    // returns a pointer to a static passwd struct (or null).
    let pw = unsafe { libc::getpwnam(c_name.as_ptr()) };
    if pw.is_null() {
        anyhow::bail!("unknown user: {user}");
    }
    // SAFETY: `pw` is non-null and points to a valid passwd struct.
    Ok(unsafe { (*pw).pw_uid })
}
