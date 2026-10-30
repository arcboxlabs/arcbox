//! Readiness streaming with the recovery guest's runtime-start restriction.

use std::time::Duration;

use anyhow::Result;
use buffa::Message as _;
use tokio::io::AsyncWrite;

use crate::agent::Guest;
use crate::rpc::{ErrorResponse, RpcResponse, write_message, write_response};

pub(super) async fn watch<S>(
    stream: &mut S,
    req: arcbox_connect::v1::WatchReadinessRequest,
    trace_id: &str,
    guest: Guest,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    use arcbox_connect::v1::ReadinessEvent;
    use arcbox_connect::v1::readiness_event::Kind;

    if guest == Guest::StorageRecovery && req.start_runtime_if_needed {
        write_response(
            stream,
            &RpcResponse::Error(ErrorResponse::new(
                403,
                "storage recovery mode cannot start the runtime",
            )),
            trace_id,
        )
        .await?;
        return Ok(());
    }

    write_readiness_event(
        stream,
        trace_id,
        ReadinessEvent {
            kind: Kind::AgentReady.into(),
            detail: "agent ready".to_string(),
            ..Default::default()
        },
    )
    .await?;

    if !req.start_runtime_if_needed {
        return Ok(());
    }

    write_readiness_event(
        stream,
        trace_id,
        ReadinessEvent {
            kind: Kind::RuntimeStarting.into(),
            detail: "ensuring guest runtime".to_string(),
            ..Default::default()
        },
    )
    .await?;

    let timeout = Duration::from_millis(u64::from(req.timeout_ms).max(1));
    let event = super::super::runtime::watch_runtime_readiness(timeout).await;
    write_readiness_event(stream, trace_id, event).await
}

async fn write_readiness_event<S>(
    stream: &mut S,
    trace_id: &str,
    event: arcbox_connect::v1::ReadinessEvent,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_message(
        stream,
        crate::rpc::MessageType::ReadinessEvent,
        trace_id,
        &event.encode_to_vec(),
    )
    .await
}
