//! Machine-level command execution.
//!
//! Handles [`MessageType::MachineExecRequest`] by spawning the command in the
//! agent's own mount namespace — which for a distro machine is the machine's
//! overlay root. A `login` request runs the account's shell the way sshd
//! would (`process.rs`, `login_session.rs`).
//!
//! Two modes share the entry point:
//! - **piped** (`tty == false`): stdin closed, stdout/stderr streamed as
//!   separate [`MessageType::MachineExecOutput`] frames;
//! - **interactive** (`tty == true`): the command runs as a session leader on
//!   a PTY (primitives from `arcbox-pty`); output is a single merged stream,
//!   and the host feeds [`MessageType::MachineExecInput`] /
//!   [`MessageType::MachineExecResize`] frames on the same connection.
//!
//! Both end with a `done == true` frame carrying the exit code, or the
//! signal that ended the process.

mod process;

use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitStatus, Stdio};

use anyhow::Context;
use arcbox_connect::v1::MachineExecRequest;
use buffa::Message;
use nix::sys::signal::Signal;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, ChildStderr, ChildStdout};

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
    let req = MachineExecRequest::decode_from_slice(payload)
        .context("failed to decode MachineExecRequest")?;

    let spec = match ProcessSpec::resolve(&req) {
        Ok(spec) => spec,
        Err(err) => {
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    if req.tty {
        tty_session(stream, trace_id, &req, spec).await
    } else {
        piped_session(stream, trace_id, spec).await
    }
}

/// Piped session: stdout and stderr stream as separate frames; stdin is
/// /dev/null.
async fn piped_session<S>(stream: &mut S, trace_id: &str, spec: ProcessSpec) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
        Ok(child) => child,
        Err(e) => {
            let err = spec.spawn_error(e);
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    let stdout = child.stdout.take().context("stdout not piped")?;
    let stderr = child.stderr.take().context("stderr not piped")?;
    let status = pump_pipes(stream, trace_id, stdout, stderr, &mut child).await?;
    write_exit(stream, trace_id, status).await
}

/// Streams stdout and stderr until both close, then reaps the process.
async fn pump_pipes<W>(
    conn: &mut W,
    trace_id: &str,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    child: &mut Child,
) -> anyhow::Result<ExitStatus>
where
    W: AsyncWrite + Unpin,
{
    // On the heap: two stack buffers held across awaits would make every
    // session future 16 KiB larger.
    let mut stdout_buf = vec![0u8; 8192];
    let mut stderr_buf = vec![0u8; 8192];
    let mut stdout_done = false;
    let mut stderr_done = false;
    while !stdout_done || !stderr_done {
        tokio::select! {
            res = stdout.read(&mut stdout_buf), if !stdout_done => match res {
                Ok(0) => stdout_done = true,
                Ok(n) => write_output(conn, trace_id, "stdout", &stdout_buf[..n]).await?,
                Err(e) => {
                    tracing::warn!(error = %e, "machine exec stdout read error");
                    stdout_done = true;
                }
            },
            res = stderr.read(&mut stderr_buf), if !stderr_done => match res {
                Ok(0) => stderr_done = true,
                Ok(n) => write_output(conn, trace_id, "stderr", &stderr_buf[..n]).await?,
                Err(e) => {
                    tracing::warn!(error = %e, "machine exec stderr read error");
                    stderr_done = true;
                }
            },
        }
    }
    child.wait().await.context("failed to wait for exec child")
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

    let status = child.wait().await;
    let _ = reader.await;

    if !host_gone {
        let status = status.context("failed to wait for exec child")?;
        let _ = write_exit(&mut conn_wr, trace_id, status).await;
    }

    Ok(())
}

/// Writes one output frame (`stream` is `"stdout"` or `"stderr"`).
async fn write_output<W>(
    writer: &mut W,
    trace_id: &str,
    stream: &str,
    data: &[u8],
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let out = arcbox_connect::v1::MachineExecOutput {
        stream: stream.to_owned(),
        data: data.to_vec(),
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

/// Writes the final frame reporting how the process ended.
async fn write_exit<W>(writer: &mut W, trace_id: &str, status: ExitStatus) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let out = arcbox_connect::v1::MachineExecOutput {
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
