//! Exclusive authority sessions with continuous reads and joined worker cleanup.

use std::time::Instant;

use arcbox_connect::v1::{
    AgentPingResponse, InstanceAuthorityAck, OpenInstanceAuthorityResponse,
    ReplaceInstanceAuthorityRequest,
};
use arcbox_constants::wire::MessageType;
use buffa::Message;
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::transport::{AgentTransport, BLOCKING_RPC_TIMEOUT};
use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

struct Replacement {
    snapshot: ReplaceInstanceAuthorityRequest,
    applied: oneshot::Sender<()>,
}

/// Owns one authority connection. Drop cancels the retained coordinator task.
pub struct InstanceAuthoritySession {
    replacements: mpsc::Sender<Replacement>,
    opened: watch::Receiver<bool>,
    cancelled: CancellationToken,
}

impl InstanceAuthoritySession {
    /// Waits for protocol admission and exclusive Open, without publishing any roots.
    pub async fn opened(&mut self) -> Result<()> {
        self.opened
            .wait_for(|opened| *opened)
            .await
            .map(drop)
            .map_err(failure)
    }

    /// Replaces all roots and waits for the matching committed source revision.
    /// If the caller cancels this wait, drop the session and join its coordinator.
    pub async fn replace(&mut self, snapshot: ReplaceInstanceAuthorityRequest) -> Result<()> {
        let (applied, committed) = oneshot::channel();
        self.replacements
            .send(Replacement { snapshot, applied })
            .await
            .map_err(failure)?;
        committed.await.map_err(failure)
    }
}

impl Drop for InstanceAuthoritySession {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

impl AgentClient {
    /// Consumes a connected client and starts its dedicated authority session.
    /// Retain the coordinator, select it for idle EOF, and await it after dropping the session.
    pub fn instance_authority(self) -> Result<(InstanceAuthoritySession, JoinHandle<Result<()>>)> {
        let (commands, mut outgoing) = mpsc::channel::<(Bytes, Instant)>(1);
        let (incoming, mut frames) = mpsc::channel(1);
        let (replacements, mut updates) = mpsc::channel(1);
        let (ready, opened) = watch::channel(false);
        let cancelled = CancellationToken::new();
        let mut workers = JoinSet::new();
        let shutdown = match self.transport {
            AgentTransport::Async(transport) => {
                let (mut writer, mut reader) = transport.into_split().map_err(failure)?;
                let read_cancelled = cancelled.clone();
                workers.spawn(async move {
                    tokio::select! {
                        () = read_cancelled.cancelled() => Ok(()),
                        result = async {
                            loop {
                                let frame = reader.recv().await.map_err(failure)?;
                                if incoming.send(frame).await.is_err() {
                                    return Ok(());
                                }
                            }
                        } => result,
                    }
                });
                let write_cancelled = cancelled.clone();
                workers.spawn(async move {
                    tokio::select! {
                        () = write_cancelled.cancelled() => Ok(()),
                        result = async {
                            while let Some((frame, _)) = outgoing.recv().await {
                                writer.send(frame).await.map_err(failure)?;
                            }
                            Ok(())
                        } => result,
                    }
                });
                None
            }
            AgentTransport::Blocking(transport) => {
                let shutdown = transport.shutdown_handle().map_err(failure)?;
                let (mut writer, mut reader) = transport.into_split().map_err(failure)?;
                workers.spawn_blocking(move || {
                    loop {
                        let frame = reader.recv().map_err(failure)?;
                        if incoming.blocking_send(frame).is_err() {
                            return Ok(());
                        }
                    }
                });
                workers.spawn_blocking(move || {
                    while let Some((frame, deadline)) = outgoing.blocking_recv() {
                        writer.send(&frame, deadline).map_err(failure)?;
                    }
                    Ok(())
                });
                Some(shutdown)
            }
        };
        let session = InstanceAuthoritySession {
            replacements,
            opened,
            cancelled: cancelled.clone(),
        };
        let coordinator = tokio::spawn(async move {
            let mut result = tokio::select! {
                biased;
                () = cancelled.cancelled() => Ok(()),
                worker = workers.join_next() => match worker {
                    Some(Ok(Err(error))) => Err(error),
                    Some(Err(error)) => Err(failure(error)),
                    _ => Err(failure("transport worker ended")),
                },
                result = run(&commands, &mut frames, &mut updates, ready) => result,
            };
            drop(commands);
            frames.close();
            updates.close();
            cancelled.cancel();
            drop(shutdown);
            while let Some(worker) = workers.join_next().await {
                // Shutdown deliberately interrupts socket I/O; worker panics still propagate.
                if let Err(error) = worker {
                    result = Err(failure(error));
                }
            }
            result
        });
        Ok((session, coordinator))
    }
}

async fn run(
    commands: &mpsc::Sender<(Bytes, Instant)>,
    frames: &mut mpsc::Receiver<Bytes>,
    updates: &mut mpsc::Receiver<Replacement>,
    ready: watch::Sender<bool>,
) -> Result<()> {
    let ping: AgentPingResponse = exchange(
        commands,
        frames,
        MessageType::PingRequest,
        &[],
        MessageType::PingResponse,
    )
    .await?;
    AgentClient::check_agent_protocol(&ping)?;
    let _: OpenInstanceAuthorityResponse = exchange(
        commands,
        frames,
        MessageType::OpenInstanceAuthorityRequest,
        &[],
        MessageType::OpenInstanceAuthorityResponse,
    )
    .await?;
    ready.send(true).map_err(failure)?;
    loop {
        let update = tokio::select! {
            biased;
            frame = frames.recv() => {
                return Err(failure(if frame.is_some() {
                    "unsolicited response while idle"
                } else {
                    "connection closed"
                }));
            }
            update = updates.recv() => update.ok_or_else(|| failure("snapshot producer closed"))?,
        };
        let ack: InstanceAuthorityAck = exchange(
            commands,
            frames,
            MessageType::ReplaceInstanceAuthorityRequest,
            &update.snapshot.encode_to_vec(),
            MessageType::InstanceAuthorityAck,
        )
        .await?;
        if ack.authority_revision != update.snapshot.authority_revision {
            return Err(failure(
                "acknowledgement has a different authority revision",
            ));
        }
        update
            .applied
            .send(())
            .map_err(|()| failure("snapshot receiver closed"))?;
    }
}

async fn exchange<T: Message>(
    commands: &mpsc::Sender<(Bytes, Instant)>,
    frames: &mut mpsc::Receiver<Bytes>,
    request: MessageType,
    payload: &[u8],
    expected: MessageType,
) -> Result<T> {
    tokio::time::timeout(BLOCKING_RPC_TIMEOUT, async {
        commands
            .send((
                wire::build_message(request, "", payload),
                Instant::now() + BLOCKING_RPC_TIMEOUT,
            ))
            .await
            .map_err(failure)?;
        let raw = frames
            .recv()
            .await
            .ok_or_else(|| failure("connection closed"))?;
        let (kind, _, payload) = wire::parse_response(&raw)?;
        if kind == MessageType::Error as u32 {
            let (code, message) = wire::parse_error_response(&payload)?;
            return Err(EngineError::Agent { code, message });
        }
        AgentClient::expect_response_type(kind, expected)?;
        AgentClient::decode_response(&payload)
    })
    .await
    .map_err(|_| failure(format!("{request:?} timed out")))?
}

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("instance authority: {error}"))
}

#[cfg(test)]
mod tests;
