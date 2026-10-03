//! Guest→host sandbox streams under the host's window
//! (`SandboxStreamWindow` in agent.proto).
//!
//! The connection is always read to the end. A host that stops draining one
//! vsock connection stalls every new connection to that VM, so a slow
//! consumer cannot be allowed to leave frames in the socket: they queue
//! here instead, bounded by the window this side grants, and the window
//! goes back to the agent only as the consumer takes frames. The agent
//! never sends past it (`guest/arcbox-agent/src/sandbox/window.rs`).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arcbox_connect::v1::SandboxStreamWindow;
use arcbox_constants::wire::{MessageType, SANDBOX_STREAM_WINDOW};
use arcbox_transport::vsock::{Credit, VsockReceiver, VsockSender};
use buffa::Message as _;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_stream::Stream;

use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

/// How one stream's frames decode and where it ends.
pub(super) struct StreamKind<T> {
    /// The frame type the agent streams.
    pub(super) frame: MessageType,
    /// A frame type (empty payload) that ends the stream cleanly.
    pub(super) end: Option<MessageType>,
    pub(super) decode: fn(&[u8]) -> Result<T>,
    /// Whether a decoded frame is the stream's last.
    pub(super) is_last: fn(&T) -> bool,
}

/// The consumer's end of a sandbox stream: frames, then an error or the
/// last frame, then `None`. Dropping it closes the connection, which is how
/// the agent learns the host is gone.
pub struct SandboxStream<T> {
    /// Frames with the window each one took.
    frames: mpsc::UnboundedReceiver<(usize, Result<T>)>,
    /// Frames the agent may still send; the relay takes from it as frames
    /// arrive, this side gives it back as they are consumed.
    credit: Arc<Credit>,
    /// Window taken by consumed frames and not yet returned to the agent.
    unreturned: usize,
    /// The connection's writer.
    out: mpsc::UnboundedSender<Bytes>,
}

impl<T> SandboxStream<T> {
    /// The next frame, or `None` once the stream has ended.
    pub async fn recv(&mut self) -> Option<Result<T>> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T>>> {
        let (cost, item) = match self.frames.poll_recv(cx) {
            Poll::Ready(Some(next)) => next,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => return Poll::Pending,
        };
        self.consumed(cost);
        Poll::Ready(Some(item))
    }

    /// Returns window once the consumer has taken half of it, as SSH does,
    /// so the agent never waits on a consumer that keeps up.
    fn consumed(&mut self, cost: usize) {
        self.unreturned += cost;
        if self.unreturned < SANDBOX_STREAM_WINDOW as usize / 2 {
            return;
        }
        let bytes = std::mem::take(&mut self.unreturned);
        // Before the grant goes out, so frames sent against it are in
        // window. Cannot overflow: only what was taken is returned.
        if self.credit.grant(bytes).is_err() {
            return;
        }
        let grant = SandboxStreamWindow {
            bytes: bytes as u32,
            ..Default::default()
        };
        let frame =
            wire::build_message(MessageType::SandboxStreamWindow, "", &grant.encode_to_vec());
        // A closed writer means the connection already ended.
        let _ = self.out.send(frame);
    }
}

impl<T> Stream for SandboxStream<T> {
    type Item = Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_recv(cx)
    }
}

impl AgentClient {
    /// Sends `request` and runs the stream it opens over the connection.
    ///
    /// Consumes the client because the stream owns the connection.
    pub(super) async fn open_sandbox_stream<T: Send + 'static>(
        mut self,
        request: MessageType,
        payload: &[u8],
        kind: StreamKind<T>,
    ) -> Result<SandboxStream<T>> {
        if !self.connected {
            self.connect().await?;
        }
        let buf = wire::build_message(request, "", payload);
        self.transport
            .async_send(buf)
            .await
            .map_err(|source| EngineError::Transport {
                context: "failed to send sandbox stream request",
                source,
            })?;
        let (sender, mut receiver) =
            self.transport
                .into_split()
                .map_err(|source| EngineError::Transport {
                    context: "failed to split sandbox stream transport",
                    source,
                })?;

        let credit = Arc::new(Credit::new(SANDBOX_STREAM_WINDOW as usize));
        let (out, out_rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_frames(sender, out_rx));
        let (frames_tx, frames) = mpsc::unbounded_channel();
        // Reads the connection until the stream ends, an error, or the
        // consumer going away; either way the writer stops and both
        // transport halves drop, closing the connection.
        tokio::spawn({
            let credit = Arc::clone(&credit);
            async move {
                tokio::select! {
                    () = relay(&mut receiver, &frames_tx, &credit, &kind) => {}
                    () = frames_tx.closed() => {}
                }
                writer.abort();
            }
        });
        Ok(SandboxStream {
            frames,
            credit,
            unreturned: 0,
            out,
        })
    }
}

/// Writes the host's frames in the order they are queued.
async fn write_frames(mut sender: VsockSender, mut frames: mpsc::UnboundedReceiver<Bytes>) {
    while let Some(frame) = frames.recv().await {
        if sender.send(frame).await.is_err() {
            return;
        }
    }
}

/// Hands frames to the consumer, taking window for each, until the last
/// one or an error — which is handed over too — or the consumer is gone.
async fn relay<T>(
    receiver: &mut VsockReceiver,
    out: &mpsc::UnboundedSender<(usize, Result<T>)>,
    credit: &Credit,
    kind: &StreamKind<T>,
) {
    loop {
        let (cost, item) = match next_frame(receiver, credit, kind).await {
            Ok(None) => return,
            Ok(Some((cost, item))) => (cost, Ok(item)),
            Err(e) => (0, Err(e)),
        };
        let last = item.as_ref().map_or(true, kind.is_last);
        if out.send((cost, item)).is_err() || last {
            return;
        }
    }
}

/// Reads and decodes the next frame with the window it took; `None` is the
/// stream's clean end marker, an agent `Error` frame is the error.
async fn next_frame<T>(
    receiver: &mut VsockReceiver,
    credit: &Credit,
    kind: &StreamKind<T>,
) -> Result<Option<(usize, T)>> {
    let raw = receiver
        .recv()
        .await
        .map_err(|source| EngineError::Transport {
            context: "failed to receive sandbox stream frame",
            source,
        })?;
    let (resp_type, _, payload) = wire::parse_response(&raw)?;
    let cost = payload.len();
    credit.take(cost).map_err(|_| {
        EngineError::Machine("guest agent overran the sandbox stream window".into())
    })?;
    if resp_type == MessageType::Error as u32 {
        let (code, message) = wire::parse_error_response(&payload)
            .unwrap_or_else(|_| (500, "unknown error".to_string()));
        return Err(EngineError::Agent { code, message });
    }
    if kind.end.is_some_and(|end| resp_type == end as u32) {
        return Ok(None);
    }
    AgentClient::expect_response_type(resp_type, kind.frame)?;
    Ok(Some((cost, (kind.decode)(&payload)?)))
}

pub(super) fn decode_error(e: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("decode error: {e}"))
}
