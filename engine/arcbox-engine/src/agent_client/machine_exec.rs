//! Machine exec sessions: a process in the machine root, streamed over one
//! agent connection.

#[cfg(all(test, target_os = "macos"))]
mod tests;

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest, MachineExecSignal, TerminalSize};
use arcbox_constants::wire::MessageType;
use arcbox_transport::vsock::VsockReceiver;
use buffa::Message;
use bytes::Bytes;
use tokio::sync::mpsc;

use super::{AgentClient, STREAM_CHANNEL_CAPACITY, wire};
use crate::error::{EngineError, Result};

/// A single client→guest message during an interactive machine exec session.
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
    frames: mpsc::Receiver<Result<MachineExecOutput>>,
}

impl ExecSessionOutput {
    /// The next output frame; the last one carries the exit status, or is
    /// the error that ended the session. `None` after that.
    pub async fn recv(&mut self) -> Option<Result<MachineExecOutput>> {
        self.frames.recv().await
    }
}

impl AgentClient {
    /// Runs a command in the machine root (the agent's own mount namespace)
    /// with no input: a [`Self::machine_exec_session`] whose input ends at
    /// once, which the guest reads as stdin EOF.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial send fails.
    pub async fn machine_exec(self, req: MachineExecRequest) -> Result<ExecSessionOutput> {
        let (_, no_input) = mpsc::channel(1);
        self.machine_exec_session(req, no_input).await
    }

    /// Starts an exec session in the machine root (the agent's own mount
    /// namespace), PTY-backed when the request asks for a TTY.
    ///
    /// Consumes the client because the stream task requires exclusive
    /// transport access. The caller supplies a receiver of
    /// [`ExecSessionInput`]s (stdin bytes, EOF, TTY resizes, signals) and gets
    /// the session's output.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial send fails.
    pub async fn machine_exec_session(
        mut self,
        req: MachineExecRequest,
        mut input_rx: mpsc::Receiver<ExecSessionInput>,
    ) -> Result<ExecSessionOutput> {
        if !self.connected {
            self.connect().await?;
        }

        let payload = req.encode_to_vec();
        let buf = wire::build_message(MessageType::MachineExecRequest, "", &payload);
        self.transport
            .async_send(buf)
            .await
            .map_err(|source| EngineError::Transport {
                context: "failed to send exec session request",
                source,
            })?;

        let (mut sender, mut receiver) =
            self.transport
                .into_split()
                .map_err(|source| EngineError::Transport {
                    context: "failed to split exec session transport",
                    source,
                })?;

        let (out_tx, out_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

        // Input pump: channel → MachineExecInput/Resize/Signal frames. A
        // closed channel still gets an EOF frame, so a guest process
        // reading stdin cannot hang on it.
        let input_pump = tokio::spawn(async move {
            while let Some(input) = input_rx.recv().await {
                if sender.send(input.frame()).await.is_err() {
                    return;
                }
            }
            let _ = sender
                .send(ExecSessionInput::Stdin(Vec::new()).frame())
                .await;
        });

        // Output pump: MachineExecOutput frames → channel, until the final
        // frame, an error, or the consumer going away. Either way the input
        // pump is aborted and both transport halves drop, closing the
        // connection — which the guest reads as the host leaving.
        tokio::spawn(async move {
            tokio::select! {
                () = pump_exec_output(&mut receiver, &out_tx) => {}
                () = out_tx.closed() => {}
            }
            input_pump.abort();
        });

        Ok(ExecSessionOutput { frames: out_rx })
    }
}

/// Forwards a machine exec session's output frames until the final one or
/// an error — which is forwarded too — or until the consumer is gone.
async fn pump_exec_output(
    receiver: &mut VsockReceiver,
    out: &mpsc::Sender<Result<MachineExecOutput>>,
) {
    loop {
        let item = match receiver.recv().await {
            Ok(raw) => decode_exec_output(&raw),
            Err(source) => Err(EngineError::Transport {
                context: "failed to receive exec session output",
                source,
            }),
        };
        let last = item.as_ref().map_or(true, |output| output.done);
        if out.send(item).await.is_err() || last {
            return;
        }
    }
}

/// Decodes one frame of a machine exec session.
fn decode_exec_output(raw: &[u8]) -> Result<MachineExecOutput> {
    let (resp_type, _, payload) = wire::parse_response(raw)?;
    if resp_type == MessageType::Error as u32 {
        let (code, message) = wire::parse_error_response(&payload)
            .unwrap_or_else(|_| (500, "unknown error".to_string()));
        return Err(EngineError::Agent { code, message });
    }
    AgentClient::expect_response_type(resp_type, MessageType::MachineExecOutput)?;
    MachineExecOutput::decode_from_slice(&payload)
        .map_err(|e| EngineError::Machine(format!("decode error: {e}")))
}
