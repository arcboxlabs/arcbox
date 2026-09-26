//! Machine-level command execution.
//!
//! Handles [`MessageType::MachineExecRequest`] by spawning the command in the
//! agent's own mount namespace — which for a distro machine is the machine's
//! overlay root.
//!
//! Two modes share the entry point:
//! - **piped** (`tty == false`): stdin closed, stdout/stderr streamed as
//!   separate [`MessageType::MachineExecOutput`] frames;
//! - **interactive** (`tty == true`): the command runs as a session leader on
//!   a PTY (primitives from `arcbox-pty`); output is a single merged stream,
//!   and the host feeds [`MessageType::MachineExecInput`] /
//!   [`MessageType::MachineExecResize`] frames on the same connection.
//!
//! Both end with a `done == true` frame carrying the exit code.

mod process;

use std::process::Stdio;

use anyhow::Context;
use buffa::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::rpc::{ErrorResponse, MessageType, write_message};

use process::ProcessSpec;

/// Handles a machine-level exec request on the current connection.
pub(super) async fn handle_machine_exec<S>(
    stream: &mut S,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = arcbox_connect::v1::MachineExecRequest::decode_from_slice(payload)
        .context("failed to decode MachineExecRequest")?;

    let spec = match ProcessSpec::resolve(&req) {
        Ok(spec) => spec,
        Err(err) => {
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    if req.tty {
        return tty_session(stream, trace_id, &req, spec).await;
    }

    let mut cmd = spec.command();
    if let Some(run_as) = spec.run_as.clone() {
        // SAFETY: runs post-fork, pre-exec; `RunAs::apply` is
        // async-signal-safe and `run_as` was resolved before the fork.
        unsafe {
            cmd.pre_exec(move || run_as.apply());
        }
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = spec.spawn_error(e);
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
                        write_output(stream, trace_id, "stdout", &stdout_buf[..n]).await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "machine exec stdout read error");
                        stdout_done = true;
                    }
                }
            }
            res = stderr.read(&mut stderr_buf), if !stderr_done => {
                match res {
                    Ok(0) => stderr_done = true,
                    Ok(n) => {
                        write_output(stream, trace_id, "stderr", &stderr_buf[..n]).await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "machine exec stderr read error");
                        stderr_done = true;
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("failed to wait for child")?;
    let final_out = arcbox_connect::v1::MachineExecOutput {
        done: true,
        exit_code: status.code().unwrap_or(-1),
        ..Default::default()
    };
    write_message(
        stream,
        MessageType::MachineExecOutput,
        trace_id,
        &final_out.encode_to_vec(),
    )
    .await?;

    Ok(())
}

async fn write_output<S>(
    stream: &mut S,
    trace_id: &str,
    name: &str,
    data: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let out = arcbox_connect::v1::MachineExecOutput {
        stream: name.to_string(),
        data: data.to_vec(),
        ..Default::default()
    };
    write_message(
        stream,
        MessageType::MachineExecOutput,
        trace_id,
        &out.encode_to_vec(),
    )
    .await?;
    Ok(())
}

/// Interactive PTY session on the current connection.
///
/// The command runs as a session leader with the PTY slave as its
/// controlling terminal (child setup and privilege drop live in
/// `arcbox-pty`). Output is pumped from the PTY master on a blocking thread
/// (bounded channel); input/resize frames are read from the connection in
/// the same select loop, so a single writer owns the stream.
async fn tty_session<S>(
    stream: &mut S,
    trace_id: &str,
    req: &arcbox_connect::v1::MachineExecRequest,
    spec: ProcessSpec,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let size = req.tty_size.as_option().map(|s| arcbox_pty::WinSize {
        cols: u16::try_from(s.width).unwrap_or(80),
        rows: u16::try_from(s.height).unwrap_or(24),
    });
    let pty = match arcbox_pty::openpty_sized(size) {
        Ok(pty) => pty,
        Err(e) => {
            let err = ErrorResponse::new(500, format!("openpty: {e}"));
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };

    let mut cmd = spec.command();
    // The pre_exec closure dup2s the slave over stdin/stdout/stderr, so the
    // Command-level stdio configuration is irrelevant (pre_exec runs after
    // it); null keeps no stray pipes open.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // SAFETY: the closure runs post-fork pre-exec and only makes
    // async-signal-safe calls; the slave stays open in the parent until
    // after spawn.
    unsafe {
        cmd.pre_exec(arcbox_pty::child_terminal_setup(
            pty.slave.as_raw_fd(),
            spec.run_as.clone(),
        ));
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = spec.spawn_error(e);
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    // The child holds its own slave via the controlling-terminal dup2s; the
    // parent copy must close so master read hits EOF when the child exits.
    drop(pty.slave);

    let mut master_write = std::fs::File::from(pty.master.try_clone()?);
    let master_read = std::fs::File::from(pty.master.try_clone()?);
    let master_resize = pty.master;

    // Output pump: blocking PTY reads on a dedicated thread, handed to the
    // session loop over a bounded channel (backpressure caps buffering).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let reader = tokio::task::spawn_blocking(move || {
        let mut master = master_read;
        let mut buf = [0u8; 8192];
        loop {
            match master.read(&mut buf) {
                // EIO is the normal PTY EOF once the child exits.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (mut conn_rd, mut conn_wr) = tokio::io::split(stream);
    let mut host_gone = false;
    loop {
        tokio::select! {
            chunk = out_rx.recv() => match chunk {
                Some(data) => {
                    let out = arcbox_connect::v1::MachineExecOutput {
                        stream: "stdout".to_string(),
                        data,
                        ..Default::default()
                    };
                    if write_message(
                        &mut conn_wr,
                        MessageType::MachineExecOutput,
                        trace_id,
                        &out.encode_to_vec(),
                    )
                    .await
                    .is_err()
                    {
                        host_gone = true;
                        break;
                    }
                }
                // Master EOF: the child exited and released the slave.
                None => break,
            },
            frame = crate::rpc::read_message(&mut conn_rd) => match frame {
                Ok((MessageType::MachineExecInput, _, payload)) => {
                    // Empty payload signals stdin EOF; an interactive shell
                    // ends via in-band ^D, so there is nothing to forward.
                    if !payload.is_empty() {
                        // Keystroke-sized writes into the kernel PTY buffer;
                        // blocking only if the child stops draining input.
                        if let Err(e) = master_write.write_all(&payload) {
                            tracing::warn!(error = %e, "pty stdin write failed");
                        }
                    }
                }
                Ok((MessageType::MachineExecResize, _, payload)) => {
                    if let Ok(ts) = arcbox_connect::v1::TerminalSize::decode_from_slice(&payload) {
                        let _ = arcbox_pty::resize(
                            &master_resize,
                            arcbox_pty::WinSize {
                                cols: u16::try_from(ts.width).unwrap_or(80),
                                rows: u16::try_from(ts.height).unwrap_or(24),
                            },
                        );
                    }
                }
                Ok((other, _, _)) => {
                    tracing::warn!(?other, "unexpected frame during machine exec session");
                }
                Err(_) => {
                    host_gone = true;
                    break;
                }
            },
        }
    }

    if host_gone {
        // Kill the whole session (the child is its leader after setsid), not
        // just the direct child — an orphaned interactive shell must not
        // keep its descendants running.
        if let Some(pid) = child.id() {
            // SAFETY: plain kill on a process group we created.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }

    let exit_code = match child.wait().await {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => {
            tracing::warn!(error = %e, "failed to wait for exec child");
            -1
        }
    };
    let _ = reader.await;

    if !host_gone {
        let final_out = arcbox_connect::v1::MachineExecOutput {
            done: true,
            exit_code,
            ..Default::default()
        };
        let _ = write_message(
            &mut conn_wr,
            MessageType::MachineExecOutput,
            trace_id,
            &final_out.encode_to_vec(),
        )
        .await;
    }

    Ok(())
}
