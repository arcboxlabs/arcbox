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
//!   a PTY (primitives from `arcbox-pty`) and output is one merged stream.
//!   While it runs, the host sends stdin, resize and signal frames on the
//!   same connection (`control.rs`), read concurrently with the output pump;
//!   a closed connection means the host is gone and the session's process
//!   group is killed.
//!
//! Both end with a `done == true` frame carrying the exit code, or the
//! signal that ended the process.

mod control;
mod process;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitStatus, Stdio};

use anyhow::Context;
use arcbox_connect::v1::MachineExecRequest;
use buffa::Message;
use nix::sys::signal::Signal;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::mpsc;

use crate::rpc::{ErrorResponse, MessageType, write_message};

use control::Control;
use process::ProcessSpec;

/// Chunks buffered between the PTY threads and the session loop; the bound
/// is what gives each direction backpressure.
const PTY_CHANNEL_CAPACITY: usize = 16;

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

/// Interactive session: the process runs as a session leader with the PTY
/// slave as its controlling terminal (child setup and privilege drop live in
/// `arcbox-pty`), and its merged output streams as `stdout` frames.
async fn tty_session<S>(
    stream: &mut S,
    trace_id: &str,
    req: &MachineExecRequest,
    spec: ProcessSpec,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
        Ok(child) => child,
        Err(e) => {
            let err = spec.spawn_error(e);
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    // The child holds its own slave via the controlling-terminal dup2s; the
    // parent copy must close so master read hits EOF when the child exits.
    drop(pty.slave);
    let pid = child.id();

    // PTY master I/O is blocking, so each direction gets a thread of its
    // own: a child that stops reading its terminal then stalls only its
    // input, never the output or the host's resizes and signals.
    let (out_tx, out_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader = tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || pump_master_output(master, &out_tx)
    });
    let (in_tx, in_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    // Detached: it ends once `in_tx` drops or the terminal is gone.
    drop(tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || pump_master_input(master, in_rx)
    }));

    let (mut conn_rd, mut conn_wr) = tokio::io::split(stream);
    let exited = {
        let output = pump_terminal(&mut conn_wr, trace_id, out_rx, &mut child);
        let control = serve_terminal_control(&mut conn_rd, in_tx, &pty.master, pid);
        tokio::select! {
            // An error here is a failed write: the host went away.
            status = output => status.ok(),
            () = control => None,
        }
    };
    match exited {
        Some(status) => {
            let _ = reader.await;
            write_exit(&mut conn_wr, trace_id, status).await?;
        }
        None => {
            kill_session(pid);
            let _ = child.wait().await;
        }
    }
    Ok(())
}

/// Blocking PTY reads into the session loop until the master reports EOF.
fn pump_master_output(mut master: std::fs::File, out: &mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match master.read(&mut buf) {
            // EIO is the normal PTY EOF once the child exits.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Blocking PTY writes of the host's stdin.
fn pump_master_input(mut master: std::fs::File, mut input: mpsc::Receiver<Vec<u8>>) {
    while let Some(data) = input.blocking_recv() {
        if let Err(e) = master.write_all(&data) {
            tracing::debug!(error = %e, "pty stdin write ended");
            break;
        }
    }
}

/// Streams terminal output until the master reports EOF, then reaps the
/// process.
async fn pump_terminal<W>(
    conn: &mut W,
    trace_id: &str,
    mut output: mpsc::Receiver<Vec<u8>>,
    child: &mut Child,
) -> anyhow::Result<ExitStatus>
where
    W: AsyncWrite + Unpin,
{
    while let Some(data) = output.recv().await {
        write_output(conn, trace_id, "stdout", &data).await?;
    }
    child.wait().await.context("failed to wait for exec child")
}

/// Applies host instructions to an interactive session until the host
/// closes the connection.
async fn serve_terminal_control<R>(
    conn: &mut R,
    stdin: mpsc::Sender<Vec<u8>>,
    master: &OwnedFd,
    pid: Option<u32>,
) where
    R: AsyncRead + Unpin,
{
    while let Some(control) = control::next(conn).await {
        match control {
            // A closed writer means the terminal is gone; drop the input.
            Control::Stdin(data) => drop(stdin.send(data).await),
            // A terminal ends its input in-band (^D); the host's EOF has
            // nothing to close.
            Control::Eof => {}
            Control::Resize(size) => {
                if let Err(e) = arcbox_pty::resize(master, size) {
                    tracing::debug!(error = %e, "pty resize failed");
                }
            }
            Control::Signal(signal) => signal_process(pid, signal),
        }
    }
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

/// Delivers a host-requested signal to the session's process, as sshd does
/// for an SSH `signal` request (the process itself, not its group).
fn signal_process(pid: Option<u32>, signal: Signal) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        if let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            tracing::debug!(error = %e, ?signal, "machine exec signal not delivered");
        }
    }
}

/// Kills a session's whole process group: the child leads its own session,
/// so descendants must not outlive a host that went away.
fn kill_session(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
    }
}
