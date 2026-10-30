//! Owns continuous session transport workers and joins both after protocol exit or cancellation.

use std::{future::Future, time::Instant};

use arcbox_constants::wire::MessageType;
use buffa::Message;
use bytes::Bytes;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use super::transport::{AgentTransport, BLOCKING_RPC_TIMEOUT};
use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

/// Starts dedicated transport workers and joins both after the protocol or cancellation ends.
pub(super) fn start<Fut>(
    transport: AgentTransport,
    cancelled: CancellationToken,
    failure: fn(String) -> EngineError,
    protocol: impl FnOnce(mpsc::Sender<(Bytes, Instant)>, mpsc::Receiver<Bytes>) -> Fut + Send + 'static,
) -> Result<JoinHandle<Result<()>>>
where
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let (commands, mut outgoing) = mpsc::channel::<(Bytes, Instant)>(1);
    let (incoming, frames) = mpsc::channel(1);
    let mut workers = JoinSet::new();
    let shutdown = match transport {
        AgentTransport::Async(transport) => {
            let (mut writer, mut reader) = transport
                .into_split()
                .map_err(|error| failure(error.to_string()))?;
            let read_cancelled = cancelled.clone();
            workers.spawn(async move {
                tokio::select! {
                    () = read_cancelled.cancelled() => Ok(()),
                    result = async {
                        loop {
                            let frame = reader.recv().await.map_err(|error| failure(error.to_string()))?;
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
                            writer.send(frame).await.map_err(|error| failure(error.to_string()))?;
                        }
                        Ok(())
                    } => result,
                }
            });
            None
        }
        AgentTransport::Blocking(transport) => {
            let shutdown = transport
                .shutdown_handle()
                .map_err(|error| failure(error.to_string()))?;
            let (mut writer, mut reader) = transport
                .into_split()
                .map_err(|error| failure(error.to_string()))?;
            workers.spawn_blocking(move || {
                loop {
                    let frame = reader.recv().map_err(|error| failure(error.to_string()))?;
                    if incoming.blocking_send(frame).is_err() {
                        return Ok(());
                    }
                }
            });
            workers.spawn_blocking(move || {
                while let Some((frame, deadline)) = outgoing.blocking_recv() {
                    writer
                        .send(&frame, deadline)
                        .map_err(|error| failure(error.to_string()))?;
                }
                Ok(())
            });
            Some(shutdown)
        }
    };
    let coordinator = tokio::spawn(async move {
        // The protocol's channels must close before workers blocked on channel operations can join.
        let mut result = {
            let protocol = protocol(commands, frames);
            tokio::pin!(protocol);
            tokio::select! {
                biased;
                () = cancelled.cancelled() => Ok(()),
                worker = workers.join_next() => match worker {
                    Some(Ok(Err(error))) => Err(error),
                    Some(Err(error)) => Err(failure(error.to_string())),
                    _ => Err(failure("transport worker ended".into())),
                },
                result = &mut protocol => result,
            }
        };
        cancelled.cancel();
        drop(shutdown);
        while let Some(worker) = workers.join_next().await {
            // Shutdown deliberately interrupts socket I/O; worker panics still propagate.
            if let Err(error) = worker {
                result = Err(failure(error.to_string()));
            }
        }
        result
    });
    Ok(coordinator)
}

pub(super) async fn exchange<T: Message>(
    commands: &mpsc::Sender<(Bytes, Instant)>,
    frames: &mut mpsc::Receiver<Bytes>,
    request: MessageType,
    payload: &[u8],
    expected: MessageType,
    failure: fn(String) -> EngineError,
) -> Result<T> {
    tokio::time::timeout(BLOCKING_RPC_TIMEOUT, async {
        commands
            .send((
                wire::build_message(request, "", payload),
                Instant::now() + BLOCKING_RPC_TIMEOUT,
            ))
            .await
            .map_err(|error| failure(error.to_string()))?;
        let raw = frames
            .recv()
            .await
            .ok_or_else(|| failure("connection closed".into()))?;
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
