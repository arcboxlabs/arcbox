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
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::AgentClient;
use super::retained_session::{self, exchange};
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
        let (replacements, mut updates) = mpsc::channel(1);
        let (ready, opened) = watch::channel(false);
        let cancelled = CancellationToken::new();
        let coordinator = retained_session::start(
            self.transport,
            cancelled.clone(),
            failure,
            move |commands, mut frames| async move {
                run(&commands, &mut frames, &mut updates, ready).await
            },
        )?;
        let session = InstanceAuthoritySession {
            replacements,
            opened,
            cancelled,
        };
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
        failure,
    )
    .await?;
    AgentClient::check_agent_protocol(&ping)?;
    let _: OpenInstanceAuthorityResponse = exchange(
        commands,
        frames,
        MessageType::OpenInstanceAuthorityRequest,
        &[],
        MessageType::OpenInstanceAuthorityResponse,
        failure,
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
            failure,
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

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("instance authority: {error}"))
}

#[cfg(test)]
mod tests;
