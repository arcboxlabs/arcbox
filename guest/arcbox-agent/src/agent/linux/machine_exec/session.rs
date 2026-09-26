//! The loop that runs a started process's session over the connection:
//! output frames within the host's window, stdin window returned as the
//! process reads, host frames applied as they arrive — then the exit frame.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use anyhow::Context as _;
use arcbox_connect::v1::{MachineExecOutput, MachineExecWindow};
use buffa::Message as _;
use nix::sys::signal::Signal;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::control::{self, Control};
use super::flow::Flow;
use crate::rpc::{ErrorResponse, MessageType, write_message};

/// Output chunks buffered between the process's readers and the session
/// loop; the bound is what makes a process wait for the host's window.
pub(super) const OUTPUT_CHANNEL_CAPACITY: usize = 16;

/// Most output one frame carries.
pub(super) const OUTPUT_CHUNK: usize = 8192;

/// Output read from the process.
pub(super) struct Chunk {
    /// `"stdout"` or `"stderr"`.
    pub(super) stream: &'static str,
    pub(super) data: Vec<u8>,
}

/// Host stdin on its way to the process's writer.
pub(super) enum StdinItem {
    Data(Vec<u8>),
    Eof,
}

/// A started process's streams, as the session loop sees them.
pub(super) struct Streams {
    /// Output chunks; closed once every output source has ended.
    pub(super) output: mpsc::Receiver<Chunk>,
    /// Stdin for the process's writer, which never makes the loop wait.
    pub(super) stdin: mpsc::UnboundedSender<StdinItem>,
    /// Stdin byte counts the writer is done with: window to return.
    pub(super) delivered: mpsc::UnboundedReceiver<usize>,
}

/// Runs a started process's session to its end. Once the process exits
/// with its output drained, the exit frame goes out; if the host goes away
/// (or breaks flow control) first, the process group is killed instead.
pub(super) async fn run<S>(
    stream: &mut S,
    trace_id: &str,
    flow: &Flow,
    streams: Streams,
    terminal: Option<&OwnedFd>,
    child: &mut Child,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let pid = child.id();
    let (mut conn_rd, mut conn_wr) = tokio::io::split(stream);
    let exited = {
        let output = pump(
            &mut conn_wr,
            trace_id,
            flow,
            streams.output,
            streams.delivered,
            child,
        );
        let control = serve_control(&mut conn_rd, streams.stdin, flow, terminal, pid);
        tokio::select! {
            // An error here is a failed write: the host went away.
            status = output => status.ok(),
            () = control => None,
        }
    };
    match exited {
        Some(status) => write_exit(&mut conn_wr, trace_id, status).await,
        None => {
            kill_session(pid);
            let _ = child.wait().await;
            Ok(())
        }
    }
}

/// Streams output as the host's window allows and returns stdin window as
/// the writer finishes with it, until the output is drained and the process
/// has exited. Stdin window keeps flowing while output waits for the host,
/// and after the process closes its output.
async fn pump<W>(
    conn: &mut W,
    trace_id: &str,
    flow: &Flow,
    mut output: mpsc::Receiver<Chunk>,
    mut delivered: mpsc::UnboundedReceiver<usize>,
    child: &mut Child,
) -> anyhow::Result<ExitStatus>
where
    W: AsyncWrite + Unpin,
{
    // An encoded output frame waiting for window.
    let mut pending: Option<Vec<u8>> = None;
    let mut output_open = true;
    loop {
        let pending_len = pending.as_ref().map_or(0, Vec::len);
        tokio::select! {
            Some(mut len) = delivered.recv() => {
                while let Ok(more) = delivered.try_recv() {
                    len += more;
                }
                if let Some(bytes) = flow.stdin_delivered(len) {
                    write_window(conn, trace_id, bytes).await?;
                }
            }
            () = flow.reserve_output(pending_len), if pending.is_some() => {
                if let Some(frame) = pending.take() {
                    write_message(conn, MessageType::MachineExecOutput, trace_id, &frame).await?;
                }
            }
            chunk = output.recv(), if output_open && pending.is_none() => match chunk {
                Some(chunk) => pending = Some(encode_output(chunk)),
                None => output_open = false,
            },
            status = child.wait(), if !output_open && pending.is_none() => {
                return status.context("failed to wait for exec child");
            }
        }
    }
}

/// Applies host frames until the host closes the connection or breaks flow
/// control.
async fn serve_control<R>(
    conn: &mut R,
    stdin: mpsc::UnboundedSender<StdinItem>,
    flow: &Flow,
    terminal: Option<&OwnedFd>,
    pid: Option<u32>,
) where
    R: AsyncRead + Unpin,
{
    while let Some(control) = control::next(conn).await {
        let applied = match control {
            Control::Stdin(data) => flow.admit_stdin(data.len()).map(|()| {
                // The writer only ever stops with the session.
                let _ = stdin.send(StdinItem::Data(data));
            }),
            Control::Eof => {
                let _ = stdin.send(StdinItem::Eof);
                Ok(())
            }
            Control::Resize(size) => {
                if let Some(master) = terminal {
                    if let Err(e) = arcbox_pty::resize(master, size) {
                        tracing::debug!(error = %e, "pty resize failed");
                    }
                }
                Ok(())
            }
            Control::Signal(signal) => {
                signal_process(pid, signal);
                Ok(())
            }
            Control::OutputWindow(bytes) => flow.return_output(bytes),
        };
        if let Err(e) = applied {
            tracing::warn!(error = %format!("{e:#}"), "machine exec host broke flow control");
            return;
        }
    }
}

/// Reads one of the process's pipes into `chunks` until it closes.
pub(super) fn read_output<R>(mut pipe: R, stream: &'static str, chunks: mpsc::Sender<Chunk>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; OUTPUT_CHUNK];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Chunk {
                        stream,
                        data: buf[..n].to_vec(),
                    };
                    if chunks.send(chunk).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, stream, "machine exec output read error");
                    break;
                }
            }
        }
    });
}

/// Writes host stdin into the process's pipe. Every chunk is reported done
/// once written, or dropped: the process closed its stdin, the host ended
/// it, or none was attached.
pub(super) async fn write_stdin(
    mut pipe: Option<ChildStdin>,
    mut items: mpsc::UnboundedReceiver<StdinItem>,
    delivered: mpsc::UnboundedSender<usize>,
) {
    while let Some(item) = items.recv().await {
        match item {
            StdinItem::Data(data) => {
                if let Some(open) = pipe.as_mut() {
                    if open.write_all(&data).await.is_err() {
                        pipe = None;
                    }
                }
                let _ = delivered.send(data.len());
            }
            StdinItem::Eof => pipe = None,
        }
    }
}

/// [`write_stdin`] for a PTY master, whose writes block: runs on a thread
/// of its own.
pub(super) fn write_terminal(
    mut master: std::fs::File,
    mut items: mpsc::UnboundedReceiver<StdinItem>,
    delivered: mpsc::UnboundedSender<usize>,
) {
    let mut open = true;
    while let Some(item) = items.blocking_recv() {
        // A terminal ends its input in-band (^D); the host's EOF has
        // nothing to close.
        let StdinItem::Data(data) = item else {
            continue;
        };
        if open {
            if let Err(e) = master.write_all(&data) {
                tracing::debug!(error = %e, "pty stdin write ended");
                open = false;
            }
        }
        let _ = delivered.send(data.len());
    }
}

/// Grants the host `bytes` more stdin window.
pub(super) async fn write_window<W>(
    writer: &mut W,
    trace_id: &str,
    bytes: u32,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let frame = MachineExecWindow {
        bytes,
        ..Default::default()
    };
    write_message(
        writer,
        MessageType::MachineExecInputWindow,
        trace_id,
        &frame.encode_to_vec(),
    )
    .await
}

/// Answers the request with `err` instead of a session.
pub(super) async fn write_error<W>(
    writer: &mut W,
    trace_id: &str,
    err: &ErrorResponse,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_message(writer, MessageType::Error, trace_id, &err.encode()).await
}

/// The `MachineExecOutput` payload carrying `chunk`; its size is the window
/// it takes.
fn encode_output(chunk: Chunk) -> Vec<u8> {
    MachineExecOutput {
        stream: chunk.stream.to_owned(),
        data: chunk.data,
        ..Default::default()
    }
    .encode_to_vec()
}

/// Writes the final frame reporting how the process ended.
async fn write_exit<W>(writer: &mut W, trace_id: &str, status: ExitStatus) -> anyhow::Result<()>
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
