//! The sandbox messages of the agent frame protocol.
//!
//! [`handle_message`] routes one request frame to the right
//! [`SandboxService`] method and writes either a single response frame or a
//! stream of frames for streaming requests (execution attach, events, file
//! reads and watches). [`serve`] runs a whole connection of them, for a
//! composer that serves this service alone; the System VM's agent instead
//! calls [`handle_message`] from its own connection loop, beside its other
//! messages.

use arcbox_connect::v1::{AgentPingRequest, AgentPingResponse};
use buffa::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::SandboxService;
use crate::frame::{ErrorResponse, MessageType, read_message, write_message};

/// Whether `msg_type` is a request [`handle_message`] answers.
#[must_use]
pub fn is_sandbox_request(msg_type: MessageType) -> bool {
    matches!(
        msg_type,
        MessageType::SandboxCreateRequest
            | MessageType::SandboxStopRequest
            | MessageType::SandboxRemoveRequest
            | MessageType::SandboxPauseRequest
            | MessageType::SandboxResumeRequest
            | MessageType::SandboxSetLifecycleRequest
            | MessageType::SandboxCleanupPrepareRequest
            | MessageType::SandboxCleanupFinalizeRequest
            | MessageType::SandboxInspectRequest
            | MessageType::SandboxListRequest
            | MessageType::SandboxExecStartRequest
            | MessageType::SandboxExecAttachRequest
            | MessageType::SandboxStdinWriteRequest
            | MessageType::SandboxStdinStatusRequest
            | MessageType::SandboxExecSignalRequest
            | MessageType::SandboxExecResizeRequest
            | MessageType::SandboxExecWaitRequest
            | MessageType::SandboxExecListRequest
            | MessageType::SandboxWaitForPortRequest
            | MessageType::SandboxEventsRequest
            | MessageType::WatchSandboxCleanupRequest
            | MessageType::SandboxFileReadRequest
            | MessageType::SandboxFileWriteRequest
            | MessageType::SandboxFileStatRequest
            | MessageType::SandboxFileListDirRequest
            | MessageType::SandboxFileMakeDirRequest
            | MessageType::SandboxFileRemoveRequest
            | MessageType::SandboxFileMoveRequest
            | MessageType::SandboxFileWatchRequest
            | MessageType::SandboxPortForwardRequest
            | MessageType::SandboxPortForwardRemoveRequest
            | MessageType::SandboxCheckpointRequest
            | MessageType::SandboxRestoreRequest
            | MessageType::SandboxListSnapshotsRequest
            | MessageType::SandboxDeleteSnapshotRequest
            | MessageType::SandboxTemplateBuildRequest
            | MessageType::SandboxTemplatePublishRequest
            | MessageType::SandboxTemplateGetRequest
            | MessageType::SandboxTemplateListRequest
            | MessageType::SandboxTemplateDeleteRequest
    )
}

/// Serves one connection: a `Ping` handshake answered with this build's
/// protocol versions, then sandbox requests until the peer closes.
///
/// For a composer whose clients reach the service alone, over a socket of
/// their own. Each request runs to completion before the next frame is
/// read, as on the agent's vsock connections.
pub async fn serve<S>(svc: &SandboxService, mut stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let (msg_type, trace_id, payload) = match read_message(&mut stream).await {
            Ok(frame) => frame,
            // The client closing between requests is the normal end.
            Err(_) => return Ok(()),
        };
        match msg_type {
            MessageType::PingRequest => {
                let request = AgentPingRequest::decode_from_slice(&payload)
                    .map_err(|e| anyhow::anyhow!("decode ping: {e}"))?;
                let response = AgentPingResponse {
                    message: request.message,
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                    ..Default::default()
                };
                write_message(
                    &mut stream,
                    MessageType::PingResponse,
                    &trace_id,
                    &response.encode_to_vec(),
                )
                .await?;
            }
            msg_type if is_sandbox_request(msg_type) => {
                handle_message(svc, &mut stream, msg_type, &trace_id, &payload).await?;
            }
            _ => {
                send_sandbox_error(
                    &mut stream,
                    &trace_id,
                    400,
                    "this endpoint serves sandbox requests only",
                )
                .await?;
            }
        }
    }
}

/// Dispatches one sandbox request.
///
/// Non-streaming requests (CRUD, snapshots) write a single response frame
/// and return. Streaming requests (attach, events, reads, watches) write
/// frames until the stream ends.
pub async fn handle_message<S>(
    svc: &SandboxService,
    stream: &mut S,
    msg_type: MessageType,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match msg_type {
        // -----------------------------------------------------------------
        // CRUD
        // -----------------------------------------------------------------
        MessageType::SandboxCreateRequest => match svc.create(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxCreateResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxStopRequest => {
            use arcbox_connect::sandbox_v1::StopSandboxRequest;
            use arcbox_connect::v1::SandboxCleanupResponse;
            let req = match StopSandboxRequest::decode_from_slice(payload) {
                Ok(req) => req,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            let sandbox_id = req.id.clone();
            let _operation = svc.lock_operation(&req.id).await;
            match svc.stop_request(req).await {
                Ok(()) => match svc.pending_cleanup_ticket(&sandbox_id).await {
                    Ok(ticket) => {
                        let response = SandboxCleanupResponse {
                            ticket: ticket.into(),
                            ..Default::default()
                        };
                        write_message(
                            stream,
                            MessageType::SandboxStopResponse,
                            trace_id,
                            &response.encode_to_vec(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        send_sandbox_error(
                            stream,
                            trace_id,
                            error.status_code(),
                            &error.to_string(),
                        )
                        .await?;
                    }
                },
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            }
        }
        MessageType::SandboxRemoveRequest => {
            use arcbox_connect::sandbox_v1::RemoveSandboxRequest;
            use arcbox_connect::v1::SandboxCleanupResponse;
            let req = match RemoveSandboxRequest::decode_from_slice(payload) {
                Ok(req) => req,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            let sandbox_id = req.id.clone();
            let _operation = svc.lock_operation(&req.id).await;
            match svc.remove_request(req).await {
                Ok(()) => match svc.pending_cleanup_ticket(&sandbox_id).await {
                    Ok(ticket) => {
                        let response = SandboxCleanupResponse {
                            ticket: ticket.into(),
                            ..Default::default()
                        };
                        write_message(
                            stream,
                            MessageType::SandboxRemoveResponse,
                            trace_id,
                            &response.encode_to_vec(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        send_sandbox_error(
                            stream,
                            trace_id,
                            error.status_code(),
                            &error.to_string(),
                        )
                        .await?;
                    }
                },
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            }
        }
        MessageType::SandboxPauseRequest => {
            use arcbox_connect::sandbox_v1::PauseSandboxRequest;
            use arcbox_connect::v1::SandboxCleanupResponse;
            let req = match PauseSandboxRequest::decode_from_slice(payload) {
                Ok(req) => req,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            let sandbox_id = req.id.clone();
            let _operation = svc.lock_operation(&req.id).await;
            match svc.pause_request(req).await {
                // Pause quarantines the network exactly like Stop, so it
                // answers with the same durable cleanup ticket for the
                // daemon's host-side forwarding cleanup.
                Ok(()) => match svc.pending_cleanup_ticket(&sandbox_id).await {
                    Ok(ticket) => {
                        let response = SandboxCleanupResponse {
                            ticket: ticket.into(),
                            ..Default::default()
                        };
                        write_message(
                            stream,
                            MessageType::SandboxPauseResponse,
                            trace_id,
                            &response.encode_to_vec(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        send_sandbox_error(
                            stream,
                            trace_id,
                            error.status_code(),
                            &error.to_string(),
                        )
                        .await?;
                    }
                },
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            }
        }
        MessageType::SandboxResumeRequest => {
            use arcbox_connect::v1::SandboxResumeCommand;
            let req = match SandboxResumeCommand::decode_from_slice(payload) {
                Ok(req) => req,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            let _operation = svc.lock_operation(&req.id).await;
            match svc.resume_request(req).await {
                Ok(resp) => {
                    write_message(
                        stream,
                        MessageType::SandboxResumeResponse,
                        trace_id,
                        &resp.encode_to_vec(),
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            }
        }
        MessageType::SandboxSetLifecycleRequest => {
            use arcbox_connect::sandbox_v1::SetLifecycleRequest;
            let req = match SetLifecycleRequest::decode_from_slice(payload) {
                Ok(req) => req,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            let _operation = svc.lock_operation(&req.id).await;
            match svc.set_lifecycle_request(req).await {
                Ok(()) => {
                    write_message(
                        stream,
                        MessageType::SandboxSetLifecycleResponse,
                        trace_id,
                        &[],
                    )
                    .await?;
                }
                Err(e) => {
                    send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
                }
            }
        }
        MessageType::SandboxCleanupPrepareRequest => {
            use arcbox_connect::v1::SandboxCleanupTicket;

            let ticket = match SandboxCleanupTicket::decode_from_slice(payload) {
                Ok(ticket) => ticket,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            // Deliberately NOT under the per-sandbox operation lock: the
            // handshake is generation-guarded by the ticket token (a stale
            // ticket fails 404/412, which the host treats as obsolete), and
            // a same-id create legitimately HOLDS the operation lock while
            // awaiting this very finalization — the warm-restore
            // probe-failure fallback removes its sandbox and then waits for
            // the network quarantine to clear. Taking the lock here
            // deadlocked that fallback (CORE-107 e2e).
            match svc.prepare_cleanup(&ticket).await {
                Ok(_) => {
                    write_message(
                        stream,
                        MessageType::SandboxCleanupPrepareResponse,
                        trace_id,
                        &[],
                    )
                    .await?;
                }
                Err(error) => {
                    send_sandbox_error(stream, trace_id, error.status_code(), &error.to_string())
                        .await?;
                }
            }
        }
        MessageType::SandboxCleanupFinalizeRequest => {
            use arcbox_connect::v1::SandboxCleanupTicket;

            let ticket = match SandboxCleanupTicket::decode_from_slice(payload) {
                Ok(ticket) => ticket,
                Err(error) => {
                    send_sandbox_error(stream, trace_id, 400, &error.to_string()).await?;
                    return Ok(());
                }
            };
            // No operation lock, same reason as the Prepare arm above: the
            // token CAS is the guard, and a create awaiting quarantine
            // release holds the lock this would need.
            let result = async {
                let sandbox_ip = svc.prepare_cleanup(&ticket).await?;
                svc.hooks.release_forwards(&ticket, sandbox_ip).await?;
                svc.finalize_cleanup(&ticket).await
            }
            .await;
            match result {
                Ok(()) => {
                    write_message(
                        stream,
                        MessageType::SandboxCleanupFinalizeResponse,
                        trace_id,
                        &[],
                    )
                    .await?;
                }
                Err(error) => {
                    send_sandbox_error(stream, trace_id, error.status_code(), &error.to_string())
                        .await?;
                }
            }
        }
        MessageType::SandboxInspectRequest => match svc.inspect(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxInspectResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxListRequest => match svc.list(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxListResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        // -----------------------------------------------------------------
        // Executions (execution redesign, CORE-55/56)
        // -----------------------------------------------------------------
        MessageType::SandboxExecStartRequest => match svc.start_execution(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxExecStartResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxExecAttachRequest => {
            svc.handle_attach(stream, trace_id, payload).await?;
        }
        MessageType::SandboxStdinWriteRequest => match svc.write_stdin(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxStdinStatus,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxStdinStatusRequest => match svc.stdin_status(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxStdinStatus,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxExecSignalRequest => match svc.signal_execution(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxExecSignalResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxExecResizeRequest => match svc.resize_execution(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxExecResizeResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxExecWaitRequest => match svc.wait_execution(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxExecWaitResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxExecListRequest => match svc.list_executions(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxExecListResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxWaitForPortRequest => match svc.wait_for_port(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxWaitForPortResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        // -----------------------------------------------------------------
        // Streaming: Events
        // -----------------------------------------------------------------
        MessageType::SandboxEventsRequest => {
            svc.handle_events(stream, trace_id, payload).await?;
        }
        MessageType::WatchSandboxCleanupRequest => {
            svc.handle_cleanup_events(stream, trace_id, payload).await?;
        }
        // -----------------------------------------------------------------
        // File I/O
        // -----------------------------------------------------------------
        MessageType::SandboxFileReadRequest => {
            svc.handle_read_file(stream, trace_id, payload).await?;
        }
        MessageType::SandboxFileWriteRequest => {
            svc.handle_write_file(stream, trace_id, payload).await?;
        }
        MessageType::SandboxFileStatRequest => match svc.stat_file(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxFileStatResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxFileListDirRequest => match svc.list_dir(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxFileListDirResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxFileMakeDirRequest => match svc.make_dir(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxFileMakeDirResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxFileRemoveRequest => match svc.remove_entry(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxFileRemoveResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxFileMoveRequest => match svc.move_entry(payload).await {
            Ok(()) => {
                write_message(stream, MessageType::SandboxFileMoveResponse, trace_id, &[]).await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxFileWatchRequest => {
            svc.handle_watch_dir(stream, trace_id, payload).await?;
        }
        // -----------------------------------------------------------------
        // Port forwarding
        // -----------------------------------------------------------------
        MessageType::SandboxPortForwardRequest => {
            handle_port_forward(stream, svc, trace_id, payload).await?;
        }
        MessageType::SandboxPortForwardRemoveRequest => {
            handle_port_forward_remove(stream, svc, trace_id, payload).await?;
        }
        // -----------------------------------------------------------------
        // Snapshots
        // -----------------------------------------------------------------
        MessageType::SandboxCheckpointRequest => match svc.checkpoint(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxCheckpointResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxRestoreRequest => match svc.restore(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxRestoreResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxListSnapshotsRequest => match svc.list_snapshots(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxListSnapshotsResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxDeleteSnapshotRequest => match svc.delete_snapshot(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxDeleteSnapshotResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        // -----------------------------------------------------------------
        // Template catalog (CORE-107)
        // -----------------------------------------------------------------
        MessageType::SandboxTemplateBuildRequest => match svc.build_template(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxTemplateBuildResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxTemplatePublishRequest => match svc.publish_template(payload).await {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxTemplatePublishResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxTemplateGetRequest => match svc.get_template(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxTemplateGetResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxTemplateListRequest => match svc.list_templates(payload) {
            Ok(resp) => {
                write_message(
                    stream,
                    MessageType::SandboxTemplateListResponse,
                    trace_id,
                    &resp.encode_to_vec(),
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        MessageType::SandboxTemplateDeleteRequest => match svc.delete_template(payload).await {
            Ok(()) => {
                write_message(
                    stream,
                    MessageType::SandboxTemplateDeleteResponse,
                    trace_id,
                    &[],
                )
                .await?;
            }
            Err(e) => {
                send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
            }
        },
        _ => {
            send_sandbox_error(stream, trace_id, 400, "unrecognised sandbox message type").await?;
        }
    }

    Ok(())
}

/// Write a single `Error` frame back to the caller.
///
/// Also logs the failure: the frame is the only copy of the error, and the
/// caller may have dropped the connection before reading it — without this
/// line a failed sandbox RPC leaves the service's log silent (CORE-82). The
/// request type is the `Received message type …` line just above on the same
/// connection.
async fn send_sandbox_error<S>(
    stream: &mut S,
    trace_id: &str,
    code: i32,
    message: &str,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    // `message` is tracing's reserved field for the event message itself:
    // passing it as a field renders the error text unlabelled, looking like
    // a second message spliced onto the first. `error` matches this file's
    // convention and renders quoted.
    tracing::warn!(trace_id, code, error = message, "sandbox RPC failed");
    let err = ErrorResponse::new(code, message);
    write_message(stream, MessageType::Error, trace_id, &err.encode()).await
}

/// Install a forward through the composer and answer with its guest port.
async fn handle_port_forward<S>(
    stream: &mut S,
    svc: &SandboxService,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    use arcbox_connect::v1::{SandboxPortForwardRequest, SandboxPortForwardResponse};

    let (req, sandbox_port, protocol) = match SandboxPortForwardRequest::decode_from_slice(payload)
        .map_err(|e| format!("decode error: {e}"))
        .and_then(|req| {
            let port = u16::try_from(req.sandbox_port)
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("invalid sandbox port {}", req.sandbox_port))?;
            let proto = req.protocol.as_known().unwrap_or_default();
            Ok((req, port, proto))
        }) {
        Ok(parsed) => parsed,
        Err(msg) => {
            send_sandbox_error(stream, trace_id, 400, &msg).await?;
            return Ok(());
        }
    };
    svc.wait_startup_cleanup_complete().await;
    let _operation = svc.lock_operation(&req.id).await;

    match svc
        .hooks
        .forward_port(svc, &req.id, sandbox_port, protocol)
        .await
    {
        Ok(guest_port) => {
            let resp = SandboxPortForwardResponse {
                guest_port: u32::from(guest_port),
                ..Default::default()
            };
            write_message(
                stream,
                MessageType::SandboxPortForwardResponse,
                trace_id,
                &resp.encode_to_vec(),
            )
            .await?;
        }
        Err(e) => {
            send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
        }
    }
    Ok(())
}

/// Remove a forward (idempotent) and acknowledge.
async fn handle_port_forward_remove<S>(
    stream: &mut S,
    svc: &SandboxService,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    use arcbox_connect::v1::SandboxPortForwardRemoveRequest;

    let (req, sandbox_port, protocol) =
        match SandboxPortForwardRemoveRequest::decode_from_slice(payload)
            .map_err(|e| format!("decode error: {e}"))
            .and_then(|req| {
                let port = u16::try_from(req.sandbox_port)
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| format!("invalid sandbox port {}", req.sandbox_port))?;
                let proto = req.protocol.as_known().unwrap_or_default();
                Ok((req, port, proto))
            }) {
            Ok(parsed) => parsed,
            Err(msg) => {
                send_sandbox_error(stream, trace_id, 400, &msg).await?;
                return Ok(());
            }
        };
    svc.wait_startup_cleanup_complete().await;
    let _operation = svc.lock_operation(&req.id).await;
    match svc
        .hooks
        .remove_port(svc, &req.id, sandbox_port, protocol)
        .await
    {
        Ok(()) => {
            write_message(
                stream,
                MessageType::SandboxPortForwardRemoveResponse,
                trace_id,
                &[],
            )
            .await?;
        }
        Err(e) => {
            send_sandbox_error(stream, trace_id, e.status_code(), &e.to_string()).await?;
        }
    }
    Ok(())
}
