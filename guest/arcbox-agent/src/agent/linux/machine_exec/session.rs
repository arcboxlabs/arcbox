//! The loop that runs a started process's session over the connection:
//! output frames as the process writes, host frames applied as they arrive
//! — then the exit frame.

use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use anyhow::Context as _;
use arcbox_connect::v1::MachineExecOutput;
use buffa::Message as _;
use nix::sys::signal::Signal;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::control::{self, Control};
use crate::rpc::{MessageType, write_message};

/// Chunks buffered between the process and the session loop, each way; the
/// bound is what gives each direction backpressure.
pub(super) const CHANNEL_CAPACITY: usize = 16;

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
    /// Stdin for the process's writer.
    pub(super) stdin: mpsc::Sender<StdinItem>,
}

/// Runs a started process's session to its end. Once the process exits
/// with its output drained, the exit frame goes out; if the host goes away
/// first, the process group is killed instead.
pub(super) async fn run<S>(
    stream: &mut S,
    trace_id: &str,
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
        let output = pump(&mut conn_wr, trace_id, streams.output, child);
        let control = serve_control(&mut conn_rd, streams.stdin, terminal, pid);
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

/// Streams output until every source has closed, then reaps the process.
async fn pump<W>(
    conn: &mut W,
    trace_id: &str,
    mut output: mpsc::Receiver<Chunk>,
    child: &mut Child,
) -> anyhow::Result<ExitStatus>
where
    W: AsyncWrite + Unpin,
{
    while let Some(chunk) = output.recv().await {
        write_output(conn, trace_id, chunk).await?;
    }
    child.wait().await.context("failed to wait for exec child")
}

/// Applies host frames until the host closes the connection.
async fn serve_control<R>(
    conn: &mut R,
    stdin: mpsc::Sender<StdinItem>,
    terminal: Option<&OwnedFd>,
    pid: Option<u32>,
) where
    R: AsyncRead + Unpin,
{
    while let Some(control) = control::next(conn).await {
        match control {
            // The writer only ever stops with the session.
            Control::Stdin(data) => drop(stdin.send(StdinItem::Data(data)).await),
            Control::Eof => drop(stdin.send(StdinItem::Eof).await),
            Control::Resize(size) => {
                if let Some(master) = terminal {
                    if let Err(e) = arcbox_pty::resize(master, size) {
                        tracing::debug!(error = %e, "pty resize failed");
                    }
                }
            }
            Control::Signal(signal) => signal_process(pid, signal),
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

/// Writes host stdin into the process's pipe; once the process closes its
/// stdin, the host ends it, or none was attached, stdin is dropped.
pub(super) async fn write_stdin(
    mut pipe: Option<ChildStdin>,
    mut items: mpsc::Receiver<StdinItem>,
) {
    while let Some(item) = items.recv().await {
        match item {
            StdinItem::Data(data) => {
                if let Some(open) = pipe.as_mut() {
                    if open.write_all(&data).await.is_err() {
                        pipe = None;
                    }
                }
            }
            StdinItem::Eof => pipe = None,
        }
    }
}

/// Writes one output frame.
async fn write_output<W>(writer: &mut W, trace_id: &str, chunk: Chunk) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let out = MachineExecOutput {
        stream: chunk.stream.to_owned(),
        data: chunk.data,
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
