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
//! While the process runs, the host sends stdin, resize, signal and window
//! frames on the same connection (`control.rs`), applied beside the output
//! pump (`session.rs`) under the flow control the request asks for
//! (`flow.rs`); a closed connection means the host is gone and the
//! session's process group is killed. Both modes end with a `done == true`
//! frame carrying the exit code, or the signal that ended the process.

mod control;
mod debug;
mod flow;
mod login_path;
mod process;
mod session;
mod tcp;

pub(super) use tcp::handle_tcp_connect;

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::Stdio;

use anyhow::Context;
use arcbox_connect::v1::MachineExecRequest;
use arcbox_pty::RunAs;
use buffa::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::rpc::{ErrorResponse, MessageType};

use flow::Flow;
use process::ProcessSpec;
use session::{Chunk, OUTPUT_CHANNEL_CAPACITY, OUTPUT_CHUNK, OutFrame, Streams};

/// Handles a machine-level exec request; the session owns the rest of the
/// connection.
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

    let flow = match Flow::new(req.output_window) {
        Ok(flow) => flow,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    // Before any other frame, so the host knows flow control is on.
    if let Some(window) = flow.initial_stdin_window() {
        session::write_window(stream, trace_id, window).await?;
    }
    let spec = match ProcessSpec::resolve(&req).await {
        Ok(spec) => spec,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    run_session(stream, trace_id, &req, spec, &flow, None).await
}

/// Handles a container-debug exec request: like [`handle_machine_exec`], but
/// the process enters the target container's namespaces first (see the
/// [`debug`] module for the mechanism and its trade-offs). Takes a
/// `MachineExecRequest` with the target in [`MachineExecRequest::container`]
/// and streams the output back as `DebugExecResponse` frames.
pub(super) async fn handle_debug_exec<S>(
    stream: &mut S,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = MachineExecRequest::decode_from_slice(payload)
        .context("failed to decode DebugExecRequest")?;

    let flow = match Flow::new(req.output_window) {
        Ok(flow) => flow,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    // Before any other frame, so the host knows flow control is on.
    if let Some(window) = flow.initial_stdin_window() {
        session::write_window(stream, trace_id, window).await?;
    }
    // Resolve (and open handles to) the target's namespaces before spawning:
    // a missing or stopped container fails here with a clear error rather than
    // as an opaque spawn failure.
    let nsenter = match debug::NsEnter::resolve(&req.container).await {
        Ok(ns) => ns,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    let spec = match ProcessSpec::resolve(&req).await {
        Ok(spec) => spec,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    run_session(stream, trace_id, &req, spec, &flow, Some(nsenter)).await
}

/// Dispatches an exec to the piped or interactive path. `nsenter` is `Some`
/// for a container-debug session (its namespaces are entered in the child
/// pre-exec step) and `None` for a machine-root exec.
async fn run_session<S>(
    stream: &mut S,
    trace_id: &str,
    req: &MachineExecRequest,
    spec: ProcessSpec,
    flow: &Flow,
    nsenter: Option<debug::NsEnter>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Debug-exec output is tagged as DebugExecResponse (the 0x1000 response
    // of DebugExecRequest); a machine-root exec stays MachineExecOutput.
    let out = OutFrame {
        trace_id,
        msg_type: if nsenter.is_some() {
            MessageType::DebugExecResponse
        } else {
            MessageType::MachineExecOutput
        },
    };
    if req.tty {
        tty_session(stream, out, req, spec, flow, nsenter).await
    } else {
        piped_session(stream, out, req, spec, flow, nsenter).await
    }
}

/// Piped session: stdout and stderr stream as separate frames.
async fn piped_session<S>(
    stream: &mut S,
    out: OutFrame<'_>,
    req: &MachineExecRequest,
    spec: ProcessSpec,
    flow: &Flow,
    nsenter: Option<debug::NsEnter>,
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
    // SAFETY: runs post-fork, pre-exec; the namespace entry, setsid and the
    // credential syscalls are async-signal-safe, and `run_as` and any
    // namespace handles were resolved before the fork.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(ns) = &nsenter {
                ns.apply()?;
            }
            // Lead a session of its own, as sshd's children do, so the whole
            // tree can be killed when the host goes away.
            nix::unistd::setsid()?;
            run_as.as_ref().map_or(Ok(()), RunAs::apply)
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return session::write_error(stream, out.trace_id, &spec.spawn_error(e)).await,
    };
    let (output_tx, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let stdout = child.stdout.take().context("stdout not piped")?;
    let stderr = child.stderr.take().context("stderr not piped")?;
    session::read_output(stdout, "stdout", output_tx.clone());
    session::read_output(stderr, "stderr", output_tx);
    let (stdin, stdin_rx) = mpsc::unbounded_channel();
    let (delivered_tx, delivered) = mpsc::unbounded_channel();
    tokio::spawn(session::write_stdin(
        child.stdin.take(),
        stdin_rx,
        delivered_tx,
    ));

    let streams = Streams {
        output,
        stdin,
        delivered,
    };
    session::run(stream, out, flow, streams, None, &mut child).await
}

/// Interactive session: the process runs as a session leader with the PTY
/// slave as its controlling terminal (child setup and privilege drop live in
/// `arcbox-pty`), and its merged output streams as `stdout` frames.
async fn tty_session<S>(
    stream: &mut S,
    out: OutFrame<'_>,
    req: &MachineExecRequest,
    spec: ProcessSpec,
    flow: &Flow,
    nsenter: Option<debug::NsEnter>,
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
            return session::write_error(stream, out.trace_id, &err).await;
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
    // after spawn, and any namespace handles were resolved before the fork.
    let mut terminal = arcbox_pty::child_terminal_setup(pty.slave.as_raw_fd(), spec.run_as.clone());
    unsafe {
        cmd.pre_exec(move || {
            // Enter the container namespaces first, then set up the PTY as the
            // controlling terminal inside them.
            if let Some(ns) = &nsenter {
                ns.apply()?;
            }
            terminal()
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return session::write_error(stream, out.trace_id, &spec.spawn_error(e)).await,
    };
    // The child holds its own slave via the controlling-terminal dup2s; the
    // parent copy must close so master read hits EOF when the child exits.
    drop(pty.slave);

    // PTY master I/O is blocking, so each direction gets a thread of its
    // own: a child that stops reading its terminal then stalls only its
    // input, never the output or the host's resizes and signals.
    let (output_tx, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    drop(tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || read_terminal(master, &output_tx)
    }));
    let (stdin, stdin_rx) = mpsc::unbounded_channel();
    let (delivered_tx, delivered) = mpsc::unbounded_channel();
    // Detached: it ends once `stdin` drops with the session.
    drop(tokio::task::spawn_blocking({
        let master = std::fs::File::from(pty.master.try_clone()?);
        move || session::write_terminal(master, stdin_rx, delivered_tx)
    }));

    let streams = Streams {
        output,
        stdin,
        delivered,
    };
    session::run(stream, out, flow, streams, Some(&pty.master), &mut child).await
}

/// Blocking PTY reads into the session loop until the master reports EOF.
fn read_terminal(mut master: std::fs::File, output: &mpsc::Sender<Chunk>) {
    let mut buf = [0u8; OUTPUT_CHUNK];
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
