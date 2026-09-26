//! `direct-tcpip` channels (`ssh -L`, VS Code Remote-SSH): a TCP connection
//! opened from inside the machine, whose bytes travel like a session's
//! stdin and output.

use std::sync::Arc;

use russh::server::{ChannelOpenHandle, Msg};
use russh::{Channel, ChannelId, ChannelOpenFailure, ChannelWriteHalf};
use tokio::sync::mpsc;

use crate::host::{ExecOutput, MachineHost};
use crate::input::{Outbound, SessionInput};
use crate::session::SessionChannel;

/// Where a forward goes.
pub struct Destination {
    pub machine: String,
    pub host: String,
    pub port: u16,
}

/// Opens a forward for `channel`. The connection is made in the background
/// — it may take a while, and the SSH connection must not wait for it —
/// and `reply` answered once it is up or has failed; a refused channel's id
/// goes to `refused`, so that its state can be dropped.
pub fn open<H: MachineHost>(
    host: Arc<H>,
    destination: Destination,
    channel: Channel<Msg>,
    reply: ChannelOpenHandle,
    outbound: Arc<Outbound>,
    refused: mpsc::UnboundedSender<ChannelId>,
) -> SessionChannel {
    // Every message for the channel also reaches the handler's callbacks,
    // so the read half would have no reader.
    let (_, writer) = channel.split();
    let (input, taken) = SessionInput::new();
    let task = tokio::spawn(async move {
        let Destination {
            machine,
            host: peer,
            port,
        } = destination;
        match host.connect_tcp(&machine, &peer, port, taken).await {
            Ok(output) => {
                outbound.send(reply.accept()).await;
                forward(output, writer, &outbound).await;
            }
            Err(e) => {
                tracing::info!(
                    machine,
                    host = peer,
                    port,
                    error = %format!("{e:#}"),
                    "ssh forward could not connect"
                );
                let _ = refused.send(writer.id());
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
            }
        }
    });
    SessionChannel::running(input, task.abort_handle())
}

/// Streams the connection's bytes to the client, the peer's end of sending
/// as channel EOF, and closes the channel once the connection is closed
/// both ways — or broke.
async fn forward(mut output: impl ExecOutput, writer: ChannelWriteHalf<Msg>, outbound: &Outbound) {
    let mut eof_sent = false;
    loop {
        match output.recv().await {
            Some(Ok(frame)) => {
                if !frame.data.is_empty()
                    && outbound.send(writer.data_bytes(frame.data)).await.is_err()
                {
                    // The client is gone.
                    return;
                }
                if frame.eof && !eof_sent {
                    eof_sent = true;
                    let _ = outbound.send(writer.eof()).await;
                }
                if frame.done {
                    break;
                }
            }
            Some(Err(e)) => {
                tracing::debug!(error = %e, "ssh forward broke");
                break;
            }
            None => break,
        }
    }
    if !eof_sent {
        let _ = outbound.send(writer.eof()).await;
    }
    let _ = outbound.send(writer.close()).await;
}
