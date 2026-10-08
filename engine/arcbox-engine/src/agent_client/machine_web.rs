//! Retained machine web configuration with fixed identity and acknowledged full policies.

use std::time::Instant;

use arcbox_connect::v1::{
    AgentPingResponse, MachineWebAck, OpenMachineWebRequest, OpenMachineWebResponse,
    ReplaceMachineWebRequest,
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
    snapshot: ReplaceMachineWebRequest,
    applied: oneshot::Sender<()>,
}

/// Owns one machine configuration connection. Drop cancels its retained coordinator.
pub struct MachineWebSession {
    replacements: mpsc::Sender<Replacement>,
    opened: watch::Receiver<bool>,
    cancelled: CancellationToken,
}

impl MachineWebSession {
    /// Waits for protocol admission and exclusive ownership of the fixed machine identity.
    pub async fn opened(&mut self) -> Result<()> {
        self.opened
            .wait_for(|opened| *opened)
            .await
            .map(drop)
            .map_err(failure)
    }

    /// Replaces the full policy with a source revision greater than every previous revision.
    /// Start at revision one. If this wait is cancelled, drop the session and join its coordinator.
    pub async fn replace(&mut self, snapshot: ReplaceMachineWebRequest) -> Result<()> {
        let (applied, committed) = oneshot::channel();
        self.replacements
            .send(Replacement { snapshot, applied })
            .await
            .map_err(failure)?;
        committed.await.map_err(failure)
    }
}

impl Drop for MachineWebSession {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

impl AgentClient {
    /// Consumes a connected client and fixes the machine identity for its configuration session.
    /// Retain the coordinator, observe idle failure, and join after dropping the session.
    pub fn machine_web(
        self,
        open: OpenMachineWebRequest,
    ) -> Result<(MachineWebSession, JoinHandle<Result<()>>)> {
        let (replacements, mut updates) = mpsc::channel(1);
        let (ready, opened) = watch::channel(false);
        let cancelled = CancellationToken::new();
        let coordinator = retained_session::start(
            self.transport,
            cancelled.clone(),
            failure,
            move |commands, mut frames| async move {
                run(&commands, &mut frames, &mut updates, ready, open).await
            },
        )?;
        Ok((
            MachineWebSession {
                replacements,
                opened,
                cancelled,
            },
            coordinator,
        ))
    }
}

async fn run(
    commands: &mpsc::Sender<(Bytes, Instant)>,
    frames: &mut mpsc::Receiver<Bytes>,
    updates: &mut mpsc::Receiver<Replacement>,
    ready: watch::Sender<bool>,
    open: OpenMachineWebRequest,
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
    let _: OpenMachineWebResponse = exchange(
        commands,
        frames,
        MessageType::OpenMachineWebRequest,
        &open.encode_to_vec(),
        MessageType::OpenMachineWebResponse,
        failure,
    )
    .await?;
    ready.send(true).map_err(failure)?;
    let mut revision = 0;
    loop {
        let update = tokio::select! {
            biased;
            frame = frames.recv() => return Err(failure(if frame.is_some() {
                "unsolicited response while idle"
            } else {
                "connection closed"
            })),
            update = updates.recv() => update.ok_or_else(|| failure("policy producer closed"))?,
        };
        if update.snapshot.source_revision <= revision {
            return Err(failure("source revision must increase"));
        }
        let ack: MachineWebAck = exchange(
            commands,
            frames,
            MessageType::ReplaceMachineWebRequest,
            &update.snapshot.encode_to_vec(),
            MessageType::MachineWebAck,
            failure,
        )
        .await?;
        if ack.source_revision != update.snapshot.source_revision {
            return Err(failure("acknowledgement has a different source revision"));
        }
        revision = ack.source_revision;
        update
            .applied
            .send(())
            .map_err(|()| failure("policy receiver closed"))?;
    }
}

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("machine web: {error}"))
}

#[cfg(test)]
mod tests;
