//! Machine-level command execution.
//!
//! Handles [`MessageType::MachineExecRequest`] by spawning the command in the
//! agent's own mount namespace — which for a distro machine is the machine's
//! overlay root. A `login` request runs the account's shell the way sshd
//! would (`process.rs`, `login_session.rs`).
//!
//! Two modes share the entry point:
//! - **piped** (`tty == false`): stdout/stderr stream as separate
//!   [`MessageType::MachineExecOutput`] frames; stdin is /dev/null unless the
//!   request attaches it;
//! - **interactive** (`tty == true`): the command runs as a session leader on
//!   a PTY (primitives from `arcbox-pty`) and output is one merged stream.
//!
//! While the process runs, the host sends stdin, resize and signal frames on
//! the same connection (`control.rs`), applied beside the output pump
//! (`session.rs`); a closed connection means the host is gone and the
//! session's process group is killed. Both modes end with a `done == true` frame
//! carrying the exit code, or the signal that ended the process.

mod control;
mod process;
mod session;

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::Stdio;

use anyhow::Context;
use arcbox_connect::v1::MachineExecRequest;
use arcbox_pty::RunAs;
use buffa::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::rpc::{ErrorResponse, MessageType, write_message};

use process::ProcessSpec;
use session::{Chunk, Streams};

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
        piped_session(stream, trace_id, &req, spec).await
    }
}

/// Piped session: stdout and stderr stream as separate frames.
async fn piped_session<S>(
    stream: &mut S,
    trace_id: &str,
    req: &MachineExecRequest,
    spec: ProcessSpec,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut cmd = spec.command();
    cmd.stdin(if req.attach_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let run_as = spec.run_as.clone();
    // SAFETY: runs post-fork, pre-exec; setsid and the credential syscalls
    // are async-signal-safe, and `run_as` was resolved before the fork.
    unsafe {
        cmd.pre_exec(move || {
            // Lead a session of its own, as sshd's children do, so the whole
            // tree can be killed when the host goes away.
            nix::unistd::setsid()?;
            run_as.as_ref().map_or(Ok(()), RunAs::apply)
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let err = spec.spawn_error(e);
            write_message(stream, MessageType::Error, trace_id, &err.encode()).await?;
            return Ok(());
        }
    };
    let (output_tx, output) = mpsc::channel(session::CHANNEL_CAPACITY);
    let stdout = child.stdout.take().context("stdout not piped")?;
    let stderr = child.stderr.take().context("stderr not piped")?;
    session::read_output(stdout, "stdout", output_tx.clone());
    session::read_output(stderr, "stderr", output_tx);
    let (stdin, stdin_rx) = mpsc::channel(session::CHANNEL_CAPACITY);
    tokio::spawn(session::write_stdin(child.stdin.take(), stdin_rx));

    let streams = Streams { output, stdin };
    session::run(stream, trace_id, streams, None, &mut child).await
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

    // PTY master I/O is blocking, so each direction gets a thread of its
    // own: a child that stops reading its terminal then stalls only its
    // input, never the output or the host's resizes and signals.
    let (output_tx, output) = mpsc::channel(session::CHANNEL_CAPACITY);
    drop(tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || read_terminal(master, &output_tx)
    }));
    let (stdin, stdin_rx) = mpsc::channel(session::CHANNEL_CAPACITY);
    // Detached: it ends once `stdin` drops with the session.
    drop(tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || session::write_terminal(master, stdin_rx)
    }));

    let streams = Streams { output, stdin };
    session::run(stream, trace_id, streams, Some(&pty.master), &mut child).await
}

/// Blocking PTY reads into the session loop until the master reports EOF.
fn read_terminal(mut master: std::fs::File, output: &mpsc::Sender<Chunk>) {
    let mut buf = [0u8; session::OUTPUT_CHUNK];
    loop {
        match master.read(&mut buf) {
            // EIO is the normal PTY EOF once the child exits.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = Chunk {
                    stream: "stdout",
                    data: buf[..n].to_vec(),
                };
                if output.blocking_send(chunk).is_err() {
                    break;
                }
            }
        }
    }
}
