//! Machine exec sessions: a process in the machine root — or a TCP
//! connection opened from inside the machine — streamed over one agent
//! connection under the session's flow control (`MachineExecWindow` in
//! agent.proto).
//!
//! The connection is always read to the end. A host that stops draining one
//! vsock connection stalls every new connection to that VM, so backpressure
//! cannot come from leaving output unread: output queues here instead,
//! bounded by the window this side grants, and the window goes back to the
//! agent only as the session's consumer takes output. Stdin goes out within
//! the window the agent grants, which lets the agent keep reading too.

#[cfg(all(test, target_os = "macos"))]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arcbox_connect::v1::{
    MachineExecOutput, MachineExecRequest, MachineExecSignal, MachineExecWindow,
    MachineTcpConnectRequest, TerminalSize,
};
use arcbox_constants::wire::MessageType;
use arcbox_transport::vsock::{VsockReceiver, VsockSender};
use buffa::Message;
use bytes::Bytes;
use tokio::sync::{Semaphore, mpsc};

use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

/// Output a session lets the agent send ahead of its consumer, in encoded
/// frame bytes — frames rather than data, so a process writing a byte at a
/// time cannot queue a million frames here.
const OUTPUT_WINDOW: u32 = 1024 * 1024;

/// Largest stdin payload in one frame.
const STDIN_FRAME: usize = 32 * 1024;

/// A single client→guest message during a machine exec session.
#[derive(Debug)]
pub enum ExecSessionInput {
    /// Raw bytes for the process's stdin. An empty payload signals EOF;
    /// resizes and signals may still follow it.
    Stdin(Vec<u8>),
    /// Resize the pseudo-TTY (only meaningful for `tty = true` sessions).
    Resize {
        /// Terminal width in columns.
        width: u16,
        /// Terminal height in rows.
        height: u16,
    },
    /// Deliver a signal to the process, named without the `SIG` prefix
    /// (`"INT"`) as SSH names it — the guest numbers signals differently.
    Signal(String),
}

impl ExecSessionInput {
    /// The wire frame carrying this input to the guest.
    fn frame(&self) -> Bytes {
        match self {
            Self::Stdin(data) => wire::build_message(MessageType::MachineExecInput, "", data),
            Self::Resize { width, height } => {
                let size = TerminalSize {
                    width: u32::from(*width),
                    height: u32::from(*height),
                    ..Default::default()
                };
                wire::build_message(MessageType::MachineExecResize, "", &size.encode_to_vec())
            }
            Self::Signal(name) => {
                let signal = MachineExecSignal {
                    name: name.clone(),
                    ..Default::default()
                };
                wire::build_message(MessageType::MachineExecSignal, "", &signal.encode_to_vec())
            }
        }
    }
}

/// The output side of a machine exec session. Dropping it ends the
/// session: the connection closes, and the guest kills the process group.
pub struct ExecSessionOutput {
    /// Frames with the window each one took.
    frames: mpsc::UnboundedReceiver<(usize, Result<MachineExecOutput>)>,
    window: OutputWindow,
}

impl ExecSessionOutput {
    /// The next output frame; the last one carries the exit status, or is
    /// the error that ended the session. `None` after that.
    pub async fn recv(&mut self) -> Option<Result<MachineExecOutput>> {
        let (cost, item) = self.frames.recv().await?;
        self.window.consumed(cost);
        Some(item)
    }
}

/// The output window this side grants the agent.
struct OutputWindow {
    /// Output the agent may still send. The reader takes window as output
    /// arrives; the consumer side gives it back.
    available: Arc<AtomicUsize>,
    /// Output the consumer took that has not been returned yet.
    unreturned: usize,
    /// The connection's writer.
    frames: mpsc::UnboundedSender<Bytes>,
}

impl OutputWindow {
    /// Returns window once the consumer has taken half of it, as SSH does,
    /// so the agent never waits on a consumer that keeps up.
    fn consumed(&mut self, len: usize) {
        self.unreturned += len;
        if self.unreturned < OUTPUT_WINDOW as usize / 2 {
            return;
        }
        let bytes = std::mem::take(&mut self.unreturned);
        // Before the grant goes out, so output sent against it is in window.
        self.available.fetch_add(bytes, Ordering::AcqRel);
        let grant = MachineExecWindow {
            bytes: bytes as u32,
            ..Default::default()
        };
        let frame = wire::build_message(
            MessageType::MachineExecOutputWindow,
            "",
            &grant.encode_to_vec(),
        );
        // A closed writer means the session already ended.
        let _ = self.frames.send(frame);
    }
}

/// Takes window for an output frame that just arrived.
fn take_output_window(available: &AtomicUsize, cost: usize) -> Result<()> {
    available
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
            left.checked_sub(cost)
        })
        .map(drop)
        .map_err(|_| EngineError::Machine("guest agent overran the exec output window".into()))
}

/// The stdin window the agent grants.
struct StdinWindow {
    credit: Semaphore,
    /// What the agent granted up front: it can never have more room.
    limit: usize,
    /// Largest stdin frame that fits the window.
    frame: usize,
}

impl StdinWindow {
    fn new(granted: u32) -> Result<Self> {
        if granted == 0 {
            return Err(EngineError::Machine(
                "guest agent granted no exec stdin window".into(),
            ));
        }
        let limit = granted as usize;
        Ok(Self {
            credit: Semaphore::new(limit),
            limit,
            frame: STDIN_FRAME.min(limit),
        })
    }

    /// Waits for room for `len` bytes of stdin and takes it.
    async fn reserve(&self, len: usize) {
        // The semaphore is never closed.
        if let Ok(permit) = self.credit.acquire_many(len as u32).await {
            permit.forget();
        }
    }

    fn grant(&self, bytes: u32) -> Result<()> {
        if self.credit.available_permits() + bytes as usize > self.limit {
            return Err(EngineError::Machine(
                "guest agent returned more exec stdin window than it granted".into(),
            ));
        }
        self.credit.add_permits(bytes as usize);
        Ok(())
    }
}

impl AgentClient {
    /// Runs a command in the machine root (the agent's own mount namespace)
    /// with no input: a [`Self::machine_exec_session`] whose input ends at
    /// once, which the guest reads as stdin EOF.
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot start.
    pub async fn machine_exec(self, req: MachineExecRequest) -> Result<ExecSessionOutput> {
        let (_, no_input) = mpsc::channel(1);
        self.machine_exec_session(req, no_input).await
    }

    /// Starts an exec session in the machine root (the agent's own mount
    /// namespace), PTY-backed when the request asks for a TTY.
    ///
    /// Consumes the client because the session owns the connection. The
    /// caller supplies a receiver of [`ExecSessionInput`]s (stdin bytes,
    /// EOF, TTY resizes, signals) and gets the session's output; stdin is
    /// taken from the receiver only as fast as the process reads it.
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot start: the request fails to
    /// send, or the agent refuses it or does not speak flow control.
    pub async fn machine_exec_session(
        self,
        mut req: MachineExecRequest,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> Result<ExecSessionOutput> {
        req.output_window = OUTPUT_WINDOW;
        let request =
            wire::build_message(MessageType::MachineExecRequest, "", &req.encode_to_vec());
        self.open_session(request, input).await
    }

    /// Opens a TCP connection to `host:port` as seen from inside the
    /// machine (`"localhost"` is the machine itself), carried like an exec
    /// session: [`ExecSessionInput::Stdin`] bytes go to the peer and an
    /// empty one shuts the sending side down; the output is the peer's
    /// bytes, then an `eof` frame once it stops sending, then a `done`
    /// frame once both directions are closed.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be opened: the agent
    /// reports why (refused, timed out, unknown host) as for a refused exec.
    pub async fn machine_tcp_connect(
        self,
        host: &str,
        port: u16,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> Result<ExecSessionOutput> {
        let req = MachineTcpConnectRequest {
            host: host.to_owned(),
            port: port.into(),
            output_window: OUTPUT_WINDOW,
            ..Default::default()
        };
        let request = wire::build_message(
            MessageType::MachineTcpConnectRequest,
            "",
            &req.encode_to_vec(),
        );
        self.open_session(request, input).await
    }

    /// Sends a session's opening `request` and, once the agent grants its
    /// stdin window, runs the session over the connection.
    async fn open_session(
        mut self,
        request: Bytes,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> Result<ExecSessionOutput> {
        if !self.connected {
            self.connect().await?;
        }
        self.transport
            .async_send(request)
            .await
            .map_err(|source| EngineError::Transport {
                context: "failed to send exec session request",
                source,
            })?;

        let (sender, mut receiver) =
            self.transport
                .into_split()
                .map_err(|source| EngineError::Transport {
                    context: "failed to split exec session transport",
                    source,
                })?;
        let stdin_window = match next_frame(&mut receiver).await? {
            Frame::StdinWindow(bytes) => Arc::new(StdinWindow::new(bytes)?),
            Frame::Output { .. } => {
                return Err(EngineError::Machine(
                    "the machine's guest agent predates flow-controlled exec sessions; \
                     restart the machine to update it"
                        .into(),
                ));
            }
        };

        let (frames_tx, frames) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_frames(sender, frames));
        let input_pump = tokio::spawn(pump_input(
            input,
            frames_tx.clone(),
            Arc::clone(&stdin_window),
        ));
        let available = Arc::new(AtomicUsize::new(OUTPUT_WINDOW as usize));
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        // Reads the connection until the final frame, an error, or the
        // consumer going away; either way the other tasks stop and both
        // transport halves drop, closing the connection — which the guest
        // reads as the host leaving.
        tokio::spawn({
            let available = Arc::clone(&available);
            async move {
                tokio::select! {
                    () = pump_output(&mut receiver, &out_tx, &available, &stdin_window) => {}
                    () = out_tx.closed() => {}
                }
                input_pump.abort();
                writer.abort();
            }
        });

        Ok(ExecSessionOutput {
            frames: out_rx,
            window: OutputWindow {
                available,
                unreturned: 0,
                frames: frames_tx,
            },
        })
    }
}

/// Writes the session's frames in the order they are queued.
async fn write_frames(mut sender: VsockSender, mut frames: mpsc::UnboundedReceiver<Bytes>) {
    while let Some(frame) = frames.recv().await {
        if sender.send(frame).await.is_err() {
            return;
        }
    }
}

/// Queues the caller's input as frames, stdin only within the agent's
/// window. A closed input channel still gets an EOF frame, so a guest
/// process reading stdin cannot hang on it.
async fn pump_input(
    mut input: mpsc::Receiver<ExecSessionInput>,
    frames: mpsc::UnboundedSender<Bytes>,
    window: Arc<StdinWindow>,
) {
    while let Some(item) = input.recv().await {
        match item {
            ExecSessionInput::Stdin(data) if !data.is_empty() => {
                for chunk in data.chunks(window.frame) {
                    window.reserve(chunk.len()).await;
                    let frame = wire::build_message(MessageType::MachineExecInput, "", chunk);
                    if frames.send(frame).is_err() {
                        return;
                    }
                }
            }
            other => {
                if frames.send(other.frame()).is_err() {
                    return;
                }
            }
        }
    }
    let _ = frames.send(ExecSessionInput::Stdin(Vec::new()).frame());
}

/// Hands output frames to the consumer, taking window for each, and stdin
/// window to the input pump, until the final frame or an error — which is
/// handed over too — or until the consumer is gone.
async fn pump_output(
    receiver: &mut VsockReceiver,
    out: &mpsc::UnboundedSender<(usize, Result<MachineExecOutput>)>,
    output_window: &AtomicUsize,
    stdin_window: &StdinWindow,
) {
    loop {
        let (cost, item) = match next_frame(receiver).await {
            Ok(Frame::StdinWindow(bytes)) => match stdin_window.grant(bytes) {
                Ok(()) => continue,
                Err(e) => (0, Err(e)),
            },
            Ok(Frame::Output { output, cost }) => match take_output_window(output_window, cost) {
                Ok(()) => (cost, Ok(output)),
                Err(e) => (0, Err(e)),
            },
            Err(e) => (0, Err(e)),
        };
        let last = item.as_ref().map_or(true, |output| output.done);
        if out.send((cost, item)).is_err() || last {
            return;
        }
    }
}

/// One frame from the agent.
enum Frame {
    /// Output, and the window it takes: its encoded size, except for the
    /// final frame, which takes none.
    Output {
        output: MachineExecOutput,
        cost: usize,
    },
    StdinWindow(u32),
}

/// Reads and decodes the next frame; an agent `Error` frame is the error.
async fn next_frame(receiver: &mut VsockReceiver) -> Result<Frame> {
    let raw = receiver
        .recv()
        .await
        .map_err(|source| EngineError::Transport {
            context: "failed to receive exec session output",
            source,
        })?;
    let (resp_type, _, payload) = wire::parse_response(&raw)?;
    if resp_type == MessageType::Error as u32 {
        let (code, message) = wire::parse_error_response(&payload)
            .unwrap_or_else(|_| (500, "unknown error".to_string()));
        return Err(EngineError::Agent { code, message });
    }
    if resp_type == MessageType::MachineExecInputWindow as u32 {
        let window = MachineExecWindow::decode_from_slice(&payload).map_err(decode_error)?;
        return Ok(Frame::StdinWindow(window.bytes));
    }
    AgentClient::expect_response_type(resp_type, MessageType::MachineExecOutput)?;
    let output = MachineExecOutput::decode_from_slice(&payload).map_err(decode_error)?;
    let cost = if output.done { 0 } else { payload.len() };
    Ok(Frame::Output { output, cost })
}

fn decode_error(e: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("decode error: {e}"))
}
