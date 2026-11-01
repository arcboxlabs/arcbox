//! Sandbox RPC entry point.
//!
//! The sandbox service is initialised lazily as a process-wide singleton.
//! `handle_sandbox_message` checks the System VM's prerequisites and hands
//! each frame to `arcbox-sandbox-service`'s dispatcher, which writes either
//! a single response frame or a stream of frames.

use std::sync::{Arc, OnceLock};

use arcbox_sandbox_service::dispatch;
use arcbox_sandbox_service::frame::{ErrorResponse, MessageType, write_message};
use tokio::io::{AsyncRead, AsyncWrite};

use super::btrfs::{ensure_data_mount, ensure_no_legacy_live_sandboxes};
use super::runtime_cache::ensure_local_runtime;
use crate::sandbox::SandboxService;

/// Returns the global [`SandboxService`] singleton.
///
/// The service is initialised lazily on the first call. The nested-virt
/// prerequisite is probed first, then the persistent Btrfs data volume is
/// mounted before the manager can create state. Without `/dev/kvm` every
/// sandbox RPC is rejected with 412 (mapped to `FAILED_PRECONDITION` on the
/// host) and an actionable reason. Initialisation failures store a 503 so
/// sandbox operations degrade instead of crashing the agent.
pub(super) fn sandbox_service(
    dns: &Arc<crate::dns_server::GuestDnsServer>,
) -> Result<&'static Arc<SandboxService>, &'static (i32, String)> {
    static SERVICE: OnceLock<Result<Arc<SandboxService>, (i32, String)>> = OnceLock::new();
    SERVICE
        .get_or_init(|| {
            if let Err(reason) = crate::sandbox::probe_kvm() {
                tracing::warn!(reason, "sandbox prerequisite missing");
                return Err((412, reason));
            }
            if let Err(reason) = ensure_data_mount() {
                tracing::warn!(reason, "sandbox data volume unavailable");
                return Err((503, format!("sandbox data volume unavailable: {reason}")));
            }
            if let Err(reason) = ensure_no_legacy_live_sandboxes() {
                tracing::warn!(reason, "legacy sandbox runtime requires guest restart");
                return Err((503, reason));
            }
            let config = crate::config::load();
            match crate::sandbox::new_service(config, Arc::clone(dns)) {
                Ok(svc) => {
                    tracing::info!("sandbox service initialised");
                    Ok(svc)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sandbox service unavailable");
                    Err((503, format!("sandbox service unavailable: {e}")))
                }
            }
        })
        .as_ref()
}

/// Dispatches a sandbox RPC request.
pub(super) async fn handle_sandbox_message<S>(
    stream: &mut S,
    msg_type: MessageType,
    trace_id: &str,
    payload: &[u8],
    services: &crate::agent::Services,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(dns) = &services.dns else {
        send_sandbox_error(
            stream,
            trace_id,
            403,
            "sandbox RPC requires System VM DNS services",
        )
        .await?;
        return Ok(());
    };
    if matches!(
        msg_type,
        MessageType::SandboxCreateRequest | MessageType::SandboxRestoreRequest
    ) {
        if let Err(reason) = ensure_local_runtime().await {
            send_sandbox_error(
                stream,
                trace_id,
                503,
                &format!("sandbox runtime unavailable: {reason}"),
            )
            .await?;
            return Ok(());
        }
    }

    let svc = match sandbox_service(dns) {
        Ok(s) => Arc::clone(s),
        Err((code, reason)) => {
            send_sandbox_error(stream, trace_id, *code, reason).await?;
            return Ok(());
        }
    };
    dispatch::handle_message(&svc, stream, msg_type, trace_id, payload).await
}

/// Write a single `Error` frame for a request the service never saw.
async fn send_sandbox_error<S>(
    stream: &mut S,
    trace_id: &str,
    code: i32,
    message: &str,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    tracing::warn!(trace_id, code, error = message, "sandbox RPC failed");
    let err = ErrorResponse::new(code, message);
    write_message(stream, MessageType::Error, trace_id, &err.encode()).await
}
