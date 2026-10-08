//! Per-connection RPC dispatch and the small inline RPC handlers
//! (Ping / Shutdown / MmapReadFile).
//!
//! Sandbox / SystemInfo / EnsureRuntime / RuntimeStatus / Kubernetes* handlers
//! live in their respective modules; this file routes parsed `RpcRequest`s
//! to the right one.

use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};

use crate::agent::Guest;
use arcbox_connect::v1::AgentPingResponse as PingResponse;
#[cfg(test)]
use buffa::Message as _;

#[cfg(test)]
use crate::rpc::write_message;
use crate::rpc::{
    AGENT_VERSION, ErrorResponse, RpcRequest, RpcResponse, parse_request, read_message,
    write_response,
};

#[cfg(test)]
mod guest_trust_tests;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
pub(in crate::agent) mod instance_authority_tests;
mod readiness;

use super::disk::handle_disk_trim;
use super::kubernetes::{
    handle_delete_kubernetes, handle_kubernetes_kubeconfig, handle_kubernetes_load_balancers,
    handle_kubernetes_status, handle_start_kubernetes, handle_stop_kubernetes,
};
use super::runtime::{handle_ensure_runtime, handle_runtime_status};
use super::sandbox::handle_sandbox_message;
use super::system_info::handle_get_system_info;
use super::vsock::is_peer_closed_error;

/// Result from handling a request.
enum RequestResult {
    /// Single response.
    Single(RpcResponse),
}

/// Handles a single vsock connection.
///
/// Reads RPC requests, processes them, and writes responses.
pub(super) async fn handle_connection<S>(
    mut stream: S,
    guest: Guest,
    services: &crate::agent::Services,
    peer_cid: u32,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Assign before Ping: reconnecting must not let buffered older updates win.
    let trust_order = if peer_cid == tokio_vsock::VMADDR_CID_HOST && guest != Guest::StorageRecovery
    {
        services
            .trust
            .as_ref()
            .map(super::super::guest_trust::Store::connection_order)
            .transpose()?
    } else {
        None
    };
    loop {
        // Read the next request (V2 wire format with trace_id).
        let (msg_type, trace_id, payload) = match read_message(&mut stream).await {
            Ok(msg) => msg,
            Err(e) => {
                // Treat peer-closed errors as a clean disconnect; surface
                // anything else (parse failure, unexpected I/O) to the
                // caller so it's logged.
                if is_peer_closed_error(&e) {
                    tracing::debug!("Client disconnected");
                    return Ok(());
                }
                return Err(e);
            }
        };

        if msg_type.is_periodic_poll() {
            tracing::debug!(trace_id = %trace_id, "Received message type {:?}", msg_type);
        } else {
            tracing::info!(
                trace_id = %trace_id,
                "Received message type {:?}, payload_len={}",
                msg_type,
                payload.len()
            );
        }

        if guest == Guest::StorageRecovery && !allowed_in_recovery(msg_type) {
            write_response(
                &mut stream,
                &RpcResponse::Error(ErrorResponse::new(
                    403,
                    "storage recovery mode only permits diagnostic control requests",
                )),
                &trace_id,
            )
            .await?;
            continue;
        }

        if matches!(
            msg_type,
            crate::rpc::MessageType::ReconcileHttpsAccessRequest
                | crate::rpc::MessageType::ReconcileGuestTrustRequest
        ) && peer_cid != tokio_vsock::VMADDR_CID_HOST
        {
            write_response(
                &mut stream,
                &RpcResponse::Error(ErrorResponse::new(403, "HTTPS control requires the host")),
                &trace_id,
            )
            .await?;
            continue;
        }

        if matches!(
            msg_type,
            crate::rpc::MessageType::OpenMachineWebRequest
                | crate::rpc::MessageType::ReplaceMachineWebRequest
        ) && (guest != Guest::DistroMachine || peer_cid != tokio_vsock::VMADDR_CID_HOST)
        {
            write_response(
                &mut stream,
                &RpcResponse::Error(ErrorResponse::new(
                    403,
                    "machine web configuration requires the host and distro machine",
                )),
                &trace_id,
            )
            .await?;
            continue;
        }

        // Sandbox requests are handled separately — they bypass the normal
        // RPC request/response cycle because streaming operations hold the
        // connection open and write multiple frames.
        if msg_type.is_sandbox_request() {
            if let Err(e) =
                handle_sandbox_message(&mut stream, msg_type, &trace_id, &payload, services).await
            {
                tracing::warn!(
                    trace_id = %trace_id,
                    request = ?msg_type,
                    error = %format!("{e:#}"),
                    "sandbox handler error"
                );
            }
            continue;
        }

        // Machine-level exec streams on this connection and then ends it:
        // host frames still in flight when the process exited (stdin,
        // returned window) must not be read as requests.
        if matches!(msg_type, crate::rpc::MessageType::MachineExecRequest) {
            if let Err(e) =
                super::machine_exec::handle_machine_exec(&mut stream, &trace_id, &payload).await
            {
                tracing::warn!(trace_id = %trace_id, error = %e, "machine exec handler error");
            }
            return Ok(());
        }
        // A debug exec is a machine exec that first enters a container's
        // namespaces; it owns the connection the same way.
        if matches!(msg_type, crate::rpc::MessageType::DebugExecRequest) {
            if let Err(e) =
                super::machine_exec::handle_debug_exec(&mut stream, &trace_id, &payload).await
            {
                tracing::warn!(trace_id = %trace_id, error = %e, "debug exec handler error");
            }
            return Ok(());
        }
        // So does a TCP connection opened inside the machine.
        if matches!(msg_type, crate::rpc::MessageType::MachineTcpConnectRequest) {
            if let Err(e) =
                super::machine_exec::handle_tcp_connect(&mut stream, &trace_id, &payload).await
            {
                tracing::warn!(trace_id = %trace_id, error = %e, "machine tcp handler error");
            }
            return Ok(());
        }

        // Parse and handle the request.
        let result = match parse_request(msg_type, &payload) {
            Ok(RpcRequest::ReconcileGuestTrust(request)) => {
                RequestResult::Single(match services.trust.as_ref().zip(trust_order) {
                    Some((store, order)) => store.reconcile(order, request).await,
                    None => RpcResponse::Error(ErrorResponse::new(
                        503,
                        "guest trust installation is unavailable for this guest",
                    )),
                })
            }
            Ok(RpcRequest::OpenMachineWeb(request)) => {
                if let Some(receiver) = &services.machine_web {
                    return receiver
                        .serve(
                            &mut stream,
                            &trace_id,
                            request,
                            super::system_info::bridge_ipv4(),
                        )
                        .await;
                }
                RequestResult::Single(RpcResponse::Error(ErrorResponse::new(
                    503,
                    "machine web services are unavailable",
                )))
            }
            Ok(RpcRequest::ReplaceMachineWeb(_)) => {
                anyhow::bail!("machine web replacement requires an open connection");
            }
            Ok(RpcRequest::OpenInstanceAuthority(_)) => {
                if guest == Guest::SystemVm
                    && peer_cid == tokio_vsock::VMADDR_CID_HOST
                    && let Some(authority) = &services.authority
                {
                    return authority.serve(&mut stream, &trace_id).await;
                }
                RequestResult::Single(RpcResponse::Error(ErrorResponse::new(
                    403,
                    "instance authority requires the host and System VM services",
                )))
            }
            Ok(RpcRequest::ReplaceInstanceAuthority(_)) => {
                anyhow::bail!("instance replacement requires an open authority connection");
            }
            Ok(RpcRequest::WatchReadiness(req)) => {
                readiness::watch(&mut stream, req, &trace_id, guest).await?;
                continue;
            }
            Ok(RpcRequest::WatchMemoryPressure(req)) => {
                super::memory_pressure::handle_watch_memory_pressure(&mut stream, req, &trace_id)
                    .await?;
                continue;
            }
            Ok(RpcRequest::WatchStats(req)) => {
                super::stats::handle_watch_stats(&mut stream, req, &trace_id).await?;
                continue;
            }
            Ok(RpcRequest::WatchStorageHealth(_)) => {
                super::storage_health::watch(&mut stream, &trace_id).await?;
                return Ok(());
            }
            Ok(RpcRequest::StorageCheck(req)) => {
                // Closing this dedicated connection cancels the response and
                // kills an active offline checker.
                let mut incoming = [0_u8; 1];
                tokio::select! {
                    response = super::storage_check::handle(req, guest) => {
                        write_response(&mut stream, &response, &trace_id).await?;
                    }
                    result = stream.read(&mut incoming) => {
                        if result? != 0 {
                            anyhow::bail!("storage check connections accept only one request");
                        }
                    }
                }
                return Ok(());
            }
            Ok(request) => handle_request(request, guest, &services.web).await,
            Err(e) => {
                tracing::warn!(trace_id = %trace_id, "Failed to parse request: {}", e);
                RequestResult::Single(RpcResponse::Error(ErrorResponse::new(
                    400,
                    format!("invalid request: {}", e),
                )))
            }
        };

        // Handle the result, echoing back the trace_id in responses.
        match result {
            RequestResult::Single(response) => {
                // Write single response
                write_response(&mut stream, &response, &trace_id).await?;
            }
        }
    }
}

fn allowed_in_recovery(kind: crate::rpc::MessageType) -> bool {
    use crate::rpc::MessageType;
    matches!(
        kind,
        MessageType::PingRequest
            | MessageType::GetSystemInfoRequest
            | MessageType::ShutdownRequest
            | MessageType::WatchReadinessRequest
            | MessageType::StorageCheckRequest
    )
}

/// Handles a single RPC request.
async fn handle_request(
    request: RpcRequest,
    guest: Guest,
    web_services: &crate::domains::WebServices,
) -> RequestResult {
    match request {
        RpcRequest::OpenMachineWeb(_) | RpcRequest::ReplaceMachineWeb(_) => {
            unreachable!("machine web configuration owns the connection")
        }
        RpcRequest::OpenInstanceAuthority(_) | RpcRequest::ReplaceInstanceAuthority(_) => {
            unreachable!("instance authority owns the connection")
        }
        RpcRequest::ReconcileGuestTrust(_) => unreachable!("guest trust uses the injected store"),
        RpcRequest::Ping(req) => RequestResult::Single(handle_ping(req, guest)),
        RpcRequest::GetSystemInfo => RequestResult::Single(handle_get_system_info().await),
        RpcRequest::ReconcileHttpsAccess(request) => {
            RequestResult::Single(match web_services.reconcile_https_access(request) {
                Ok(response) => RpcResponse::ReconcileHttpsAccess(response),
                Err(error) => RpcResponse::Error(ErrorResponse::new(400, error.to_string())),
            })
        }
        RpcRequest::ListGuestWebServices(request) => {
            let hostname = if request.hostname.is_empty() {
                None
            } else {
                match request.hostname.to_ascii_lowercase().parse() {
                    Ok(name) => Some(name),
                    Err(error) => {
                        return RequestResult::Single(RpcResponse::Error(ErrorResponse::new(
                            400, error,
                        )));
                    }
                }
            };
            RequestResult::Single(RpcResponse::ListGuestWebServices(
                arcbox_connect::v1::ListGuestWebServicesResponse {
                    services: web_services.snapshot(hostname.as_ref()),
                    ..Default::default()
                },
            ))
        }
        RpcRequest::EnsureRuntime(req) => RequestResult::Single(handle_ensure_runtime(req).await),
        RpcRequest::RuntimeStatus(req) => RequestResult::Single(handle_runtime_status(req).await),
        RpcRequest::StartKubernetes(req) => {
            RequestResult::Single(handle_start_kubernetes(req).await)
        }
        RpcRequest::StopKubernetes(req) => RequestResult::Single(handle_stop_kubernetes(req).await),
        RpcRequest::DeleteKubernetes(req) => {
            RequestResult::Single(handle_delete_kubernetes(req).await)
        }
        RpcRequest::KubernetesStatus(req) => {
            RequestResult::Single(handle_kubernetes_status(req).await)
        }
        RpcRequest::KubernetesKubeconfig(req) => {
            RequestResult::Single(handle_kubernetes_kubeconfig(req).await)
        }
        RpcRequest::KubernetesLoadBalancers(req) => {
            RequestResult::Single(handle_kubernetes_load_balancers(req).await)
        }
        RpcRequest::Shutdown(req) => RequestResult::Single(handle_shutdown(req, guest)),
        RpcRequest::MmapReadFile(req) => RequestResult::Single(handle_mmap_read_file(req)),
        RpcRequest::DiskTrim(_) => RequestResult::Single(handle_disk_trim(guest).await),
        RpcRequest::StorageCheck(_) => unreachable!("storage check owns the connection"),
        RpcRequest::ContainerFsPaths(req) => {
            RequestResult::Single(handle_container_fs_paths(req).await)
        }
        RpcRequest::ImageFsPaths(req) => RequestResult::Single(handle_image_fs_paths(req).await),
        RpcRequest::EnsureNfsExport(_) => RequestResult::Single(handle_ensure_nfs_export().await),
        RpcRequest::EnsureMachineExport(req) => RequestResult::Single(
            super::machine_export::handle_ensure_machine_export(req, guest).await,
        ),
        RpcRequest::KillAgent => RequestResult::Single(handle_kill_agent()),
        RpcRequest::WatchReadiness(_) => unreachable!("watch readiness is streaming"),
        RpcRequest::WatchMemoryPressure(_) => {
            unreachable!("watch memory pressure is streaming")
        }
        RpcRequest::WatchStats(_) => unreachable!("watch stats is streaming"),
        RpcRequest::WatchStorageHealth(_) => unreachable!("storage health is streaming"),
    }
}

/// Handles a Shutdown request.
///
/// Responds immediately so the host receives the ack, then spawns the
/// shutdown sequence in a background OS thread (not a tokio task) because
/// [`crate::shutdown::poweroff`] calls blocking libc functions and never
/// returns.
fn handle_shutdown(req: arcbox_connect::v1::ShutdownRequest, guest: Guest) -> RpcResponse {
    let grace = if req.timeout_seconds == 0 {
        Duration::from_secs(u64::from(
            arcbox_constants::timeouts::GUEST_SHUTDOWN_GRACE_SECS,
        ))
    } else {
        Duration::from_secs(u64::from(req.timeout_seconds))
    };
    tracing::info!(grace_secs = grace.as_secs(), "Shutdown requested by host");
    std::thread::spawn(move || {
        // Brief delay so the response frame flushes over vsock.
        std::thread::sleep(Duration::from_millis(100));
        crate::shutdown::poweroff(grace, guest);
    });
    RpcResponse::Shutdown(arcbox_connect::v1::ShutdownResponse {
        accepted: true,
        ..Default::default()
    })
}

/// Handles a `KillAgent` request (test-only).
///
/// Exits the agent process so PID 1 (busybox init) respawns it — used by the
/// hv_e2e supervision harness to prove crash recovery without panicking the
/// kernel. Acks first, then exits after a brief delay so the response frame
/// flushes over vsock (mirrors [`handle_shutdown`]).
fn handle_kill_agent() -> RpcResponse {
    tracing::warn!("KillAgent requested by host (test-only); exiting for busybox-init respawn");
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        std::process::exit(1);
    });
    RpcResponse::KillAgent
}

/// Handles a `ContainerFsPaths` request.
///
/// Resolves the container's snapshot layer directories from containerd (the
/// snapshot key in the moby namespace is the container ID). The host reads
/// the returned guest paths through the read-only NFS export.
async fn handle_container_fs_paths(
    req: arcbox_connect::v1::ContainerFsPathsRequest,
) -> RpcResponse {
    let id = req.container_id.as_str();
    // Container IDs are 64 hex chars; bound and charset-check the untrusted
    // input before handing it to containerd as a snapshot key.
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EINVAL,
            format!("invalid container id {id:?}"),
        ));
    }

    match crate::containerd::container_snapshot_paths(id).await {
        Ok(paths) => RpcResponse::ContainerFsPaths(arcbox_connect::v1::ContainerFsPathsResponse {
            upper_dir: paths.upper_dir.unwrap_or_default(),
            lower_dirs: paths.lower_dirs,
            ..Default::default()
        }),
        Err(e) => RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EIO,
            format!("container fs paths: {e}"),
        )),
    }
}

/// Handles an `ImageFsPaths` request.
///
/// Resolves an image's layer directories from its top layer chain ID via
/// an ephemeral containerd View snapshot. The host reads the returned
/// guest paths through the read-only NFS export.
async fn handle_image_fs_paths(req: arcbox_connect::v1::ImageFsPathsRequest) -> RpcResponse {
    let chain_id = req.top_chain_id.as_str();
    // Chain IDs are `sha256:` + 64 hex chars; charset-check the untrusted
    // input before handing it to containerd as a snapshot key.
    let is_valid = chain_id
        .strip_prefix("sha256:")
        .is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()));
    if !is_valid {
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EINVAL,
            format!("invalid top chain id {chain_id:?}"),
        ));
    }

    match crate::containerd::image_snapshot_paths(chain_id).await {
        Ok(paths) => RpcResponse::ImageFsPaths(arcbox_connect::v1::ImageFsPathsResponse {
            lower_dirs: paths.lower_dirs,
            ..Default::default()
        }),
        Err(e) => RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EIO,
            format!("image fs paths: {e}"),
        )),
    }
}

/// Handles an `EnsureNfsExport` request.
///
/// Brings up the read-only NFS export of the docker data mount so the host
/// can browse it at `~/ArcBox`. The host daemon sends this only when the
/// mount is enabled, so the guest runs no nfsd unless asked. Runtime startup
/// no longer sets the export up itself, so this is the sole entry point.
async fn handle_ensure_nfs_export() -> RpcResponse {
    match super::runtime::ensure_nfs_export().await {
        Ok(notes) => RpcResponse::EnsureNfsExport(arcbox_connect::v1::EnsureNfsExportResponse {
            notes,
            ..Default::default()
        }),
        Err(e) => RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EIO,
            format!("ensure nfs export: {e}"),
        )),
    }
}

/// Sets CLOCK_REALTIME from the given timestamp (seconds since UNIX epoch).
///
/// Idempotent — safe to call more than once. Returns `true` if the clock
/// was set successfully.
pub(super) fn sync_clock_from_host(timestamp_secs: i64) -> bool {
    if timestamp_secs <= 0 {
        return false;
    }
    let ts = libc::timespec {
        tv_sec: timestamp_secs,
        tv_nsec: 0,
    };
    // SAFETY: `ts` points to a valid initialized timespec for this call,
    // and CLOCK_REALTIME is a valid clock ID on Linux guests.
    let ret = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const ts) };
    if ret != 0 {
        tracing::warn!(
            timestamp_secs,
            error = %std::io::Error::last_os_error(),
            "failed to set clock from host"
        );
        return false;
    }
    true
}

/// Handles a `MmapReadFile` request (test-only).
///
/// Opens the requested file `O_RDONLY`, calls `mmap(MAP_SHARED)`, reads
/// the mapped region into a heap buffer, then `munmap`s. When the path
/// is on a VirtioFS mount, the `mmap` call triggers `FUSE_SETUPMAPPING`
/// on the host, which exercises the DAX path end-to-end (ABX-362).
///
/// Page-alignment: `mmap` requires page-aligned offsets, so we round
/// `offset` down to the nearest page and slice the returned bytes to
/// compensate. This matches how real userspace code uses `mmap`.
fn handle_mmap_read_file(req: arcbox_connect::v1::MmapReadFileRequest) -> RpcResponse {
    use std::os::unix::io::AsRawFd;

    const PAGE_SIZE: u64 = 4096;

    if req.length == 0 {
        return RpcResponse::MmapReadFile(arcbox_connect::v1::MmapReadFileResponse::default());
    }
    // Hard cap to keep the response message size bounded.
    if req.length > 64 * 1024 * 1024 {
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            libc::EINVAL,
            "MmapReadFile length exceeds 64 MiB cap",
        ));
    }

    let file = match std::fs::OpenOptions::new().read(true).open(&req.path) {
        Ok(f) => f,
        Err(e) => {
            return RpcResponse::Error(crate::rpc::ErrorResponse::new(
                e.raw_os_error().unwrap_or(libc::EIO),
                format!("open {}: {e}", req.path),
            ));
        }
    };

    let page_base = req.offset & !(PAGE_SIZE - 1);
    let inner_off = (req.offset - page_base) as usize;
    let total_len = inner_off + req.length as usize;
    let mapped_len = total_len.div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize;

    // SAFETY: mapped_len is > 0 and page-aligned. We pass PROT_READ and a
    // valid file fd; on success we own the mapping and must munmap it.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            #[allow(clippy::cast_possible_wrap)]
            {
                page_base as libc::off_t
            },
        )
    };
    if ptr == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        return RpcResponse::Error(crate::rpc::ErrorResponse::new(
            e.raw_os_error().unwrap_or(libc::EIO),
            format!("mmap {}: {e}", req.path),
        ));
    }

    // SAFETY: [ptr + inner_off .. ptr + inner_off + req.length] is within
    // the mapping we just obtained. Copy into a heap buffer so we can
    // unmap before returning.
    let data = unsafe {
        let slice =
            std::slice::from_raw_parts(ptr.cast::<u8>().add(inner_off), req.length as usize);
        slice.to_vec()
    };
    // SAFETY: ptr/mapped_len were returned by mmap above.
    unsafe {
        libc::munmap(ptr, mapped_len);
    }

    let bytes_read = data.len() as u64;
    RpcResponse::MmapReadFile(arcbox_connect::v1::MmapReadFileResponse {
        data,
        bytes_read,
        ..Default::default()
    })
}

/// Handles a Ping request.
fn handle_ping(req: arcbox_connect::v1::AgentPingRequest, guest: Guest) -> RpcResponse {
    tracing::debug!("Ping request: {:?}", req.message);
    if guest == Guest::SystemVm
        && let Err(reason) = super::cmdline::validate_runtime_boot_contract()
    {
        return RpcResponse::Ping(PingResponse {
            message: format!("incompatible host boot contract: {reason}"),
            version: AGENT_VERSION.to_string(),
            protocol_version: 0,
            ..Default::default()
        });
    }
    sync_clock_from_host(req.timestamp_secs);
    RpcResponse::Ping(PingResponse {
        message: if req.message.is_empty() {
            "pong".to_string()
        } else {
            format!("pong: {}", req.message)
        },
        version: AGENT_VERSION.to_string(),
        protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
        ..Default::default()
    })
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::rpc::MessageType;

    #[tokio::test]
    async fn sandbox_requests_require_injected_system_services_before_runtime_access() {
        let (mut client, server) = tokio::io::duplex(4096);
        let dispatcher = tokio::spawn(async move {
            handle_connection(
                server,
                Guest::DistroMachine,
                &crate::agent::Services::default(),
                2,
            )
            .await
        });
        for kind in [
            MessageType::SandboxCreateRequest,
            MessageType::SandboxRestoreRequest,
        ] {
            write_message(&mut client, kind, "sandbox-scope", &[0xff])
                .await
                .unwrap();
            let (kind, trace, payload) =
                tokio::time::timeout(Duration::from_secs(3), read_message(&mut client))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(kind, MessageType::Error);
            assert_eq!(trace, "sandbox-scope");
            assert_eq!(ErrorResponse::decode(&payload).unwrap().code, 403);
        }
        drop(client);
        dispatcher.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn recovery_blocks_mutation_requests_before_parsing_or_dispatch() {
        let (mut client, server) = tokio::io::duplex(4096);
        let dispatcher = tokio::spawn(async move {
            handle_connection(
                server,
                Guest::StorageRecovery,
                &crate::agent::Services::default(),
                tokio_vsock::VMADDR_CID_HOST,
            )
            .await
        });
        for kind in [
            MessageType::EnsureRuntimeRequest,
            MessageType::MachineExecRequest,
            MessageType::DebugExecRequest,
            MessageType::DiskTrimRequest,
            MessageType::SandboxCreateRequest,
            MessageType::EnsureNfsExportRequest,
        ] {
            write_message(&mut client, kind, "recovery-boundary", &[0xff])
                .await
                .unwrap();
            let (kind, trace, payload) = read_message(&mut client).await.unwrap();
            assert_eq!(kind, MessageType::Error);
            assert_eq!(trace, "recovery-boundary");
            let error = ErrorResponse::decode(&payload).unwrap();
            assert_eq!(error.code, 403);
        }
        drop(client);
        dispatcher.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn recovery_readiness_does_not_require_docker_and_refuses_runtime_start() {
        let (mut client, server) = tokio::io::duplex(4096);
        let dispatcher = tokio::spawn(async move {
            handle_connection(
                server,
                Guest::StorageRecovery,
                &crate::agent::Services::default(),
                tokio_vsock::VMADDR_CID_HOST,
            )
            .await
        });
        for start_runtime_if_needed in [false, true] {
            let request = arcbox_connect::v1::WatchReadinessRequest {
                start_runtime_if_needed,
                ..Default::default()
            };
            write_message(
                &mut client,
                MessageType::WatchReadinessRequest,
                "ready",
                &request.encode_to_vec(),
            )
            .await
            .unwrap();
            let (kind, _, payload) = read_message(&mut client).await.unwrap();
            if start_runtime_if_needed {
                assert_eq!(kind, MessageType::Error);
                assert_eq!(ErrorResponse::decode(&payload).unwrap().code, 403);
            } else {
                assert_eq!(kind, MessageType::ReadinessEvent);
                assert_eq!(
                    arcbox_connect::v1::ReadinessEvent::decode_from_slice(&payload)
                        .unwrap()
                        .kind,
                    arcbox_connect::v1::readiness_event::Kind::AgentReady
                );
            }
        }
        drop(client);
        dispatcher.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod storage_check_tests {
    use super::*;
    use crate::rpc::MessageType;
    use arcbox_connect::v1::StorageCheckRequest;
    use arcbox_connect::v1::storage_check_request::Action;

    #[tokio::test]
    async fn storage_checks_echo_errors_and_close_the_dedicated_connection() {
        for (guest, action, expected_code) in [
            (
                Guest::StorageRecovery,
                buffa::EnumValue::Unknown(i32::MAX),
                400,
            ),
            (Guest::SystemVm, buffa::EnumValue::Unknown(i32::MAX), 400),
            (
                Guest::DistroMachine,
                buffa::EnumValue::Unknown(i32::MAX),
                400,
            ),
            (Guest::SystemVm, Action::OfflineCheck.into(), 403),
            (Guest::StorageRecovery, Action::VerifyWrites.into(), 403),
            (Guest::DistroMachine, Action::OfflineCheck.into(), 403),
            (Guest::DistroMachine, Action::VerifyWrites.into(), 403),
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let dispatcher = tokio::spawn(async move {
                handle_connection(
                    server,
                    guest,
                    &crate::agent::Services::default(),
                    tokio_vsock::VMADDR_CID_HOST,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                let request = StorageCheckRequest {
                    action,
                    ..Default::default()
                };
                write_message(
                    &mut client,
                    MessageType::StorageCheckRequest,
                    "storage-check-boundary",
                    &request.encode_to_vec(),
                )
                .await
                .unwrap();
                let (kind, trace, payload) = read_message(&mut client).await.unwrap();
                assert_eq!(kind, MessageType::Error);
                assert_eq!(trace, "storage-check-boundary");
                assert_eq!(ErrorResponse::decode(&payload).unwrap().code, expected_code);
                assert_eq!(client.read(&mut [0_u8; 1]).await.unwrap(), 0);
                dispatcher.await.unwrap().unwrap();
            })
            .await
            .expect("storage check response and connection close exceeded five seconds");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domains::{EndpointFacts, HttpsBackend, SelectedRoutes, WebServices};

    #[tokio::test]
    async fn https_observation_requires_host_cid_and_valid_addresses_over_the_wire() {
        use crate::rpc::MessageType;
        use arcbox_connect::v1::{ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse};
        for (cid, address, error_code) in [
            (tokio_vsock::VMADDR_CID_HOST, "192.168.64.1", None),
            (tokio_vsock::VMADDR_CID_HOST, "192.168.64.0/24", Some(400)),
            (tokio_vsock::VMADDR_CID_HOST + 1, "192.168.64.1", Some(403)),
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(async move {
                handle_connection(
                    server,
                    Guest::SystemVm,
                    &crate::agent::Services::default(),
                    cid,
                )
                .await
            });
            let request = ReconcileHttpsAccessRequest {
                observation_id: "wire-test".into(),
                host_addresses: vec![address.into()],
                enabled: true,
                ..Default::default()
            };
            write_message(
                &mut client,
                MessageType::ReconcileHttpsAccessRequest,
                "access",
                &request.encode_to_vec(),
            )
            .await
            .unwrap();
            let (kind, trace, payload) = read_message(&mut client).await.unwrap();
            assert_eq!(trace, "access");
            if let Some(code) = error_code {
                assert_eq!(kind, MessageType::Error);
                assert_eq!(ErrorResponse::decode(&payload).unwrap().code, code);
            } else {
                assert_eq!(kind, MessageType::ReconcileHttpsAccessResponse);
                assert_eq!(
                    ReconcileHttpsAccessResponse::decode_from_slice(&payload)
                        .unwrap()
                        .sequence,
                    0
                );
            }
            drop(client);
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn list_web_services_dispatches_a_framed_request_with_native_tls() {
        let dns = crate::dns_server::GuestDnsServer::new(
            Default::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let mut claims = arcbox_dns::claims::Claim::generated(
            "api.arcbox.local".parse().unwrap(),
            "a".into(),
            "container".into(),
        )
        .to_vec();
        claims.push(arcbox_dns::claims::Claim::exclusive(
            arcbox_dns::NamePattern::parse_custom("*.svc.local").unwrap(),
            "a".into(),
            "container".into(),
        ));
        dns.registry()
            .write()
            .unwrap()
            .replace("a".into(), claims, Some("172.17.0.2".parse().unwrap()))
            .unwrap();
        let services = WebServices::new(dns.registry());
        let directory = services.directory_source();
        directory.ready();
        for id in ["a", "b"] {
            let facts = EndpointFacts::from_inspect(
                &serde_json::json!({
                    "Id": id, "Name": "/api", "State": { "Pid": 1 },
                    "NetworkSettings": {
                        "SandboxKey": "/var/run/docker/netns/api",
                        "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
                    }
                }),
                dns.domain(),
            )
            .unwrap();
            services.publish(
                &facts,
                SelectedRoutes {
                    http: None,
                    https: Some(HttpsBackend::Native(8443)),
                },
            );
        }
        let (mut client, server) = tokio::io::duplex(4096);
        let dns = std::sync::Arc::new(dns);
        let services = crate::agent::Services::system(services, dns.clone());
        let task = tokio::spawn(async move {
            handle_connection(
                server,
                Guest::SystemVm,
                &services,
                tokio_vsock::VMADDR_CID_HOST,
            )
            .await
        });
        write_message(
            &mut client,
            crate::rpc::MessageType::ListGuestWebServicesRequest,
            "web-services-test",
            &arcbox_connect::v1::ListGuestWebServicesRequest::default().encode_to_vec(),
        )
        .await
        .unwrap();
        let (kind, trace, payload) = read_message(&mut client).await.unwrap();
        assert_eq!(kind, crate::rpc::MessageType::ListGuestWebServicesResponse);
        assert_eq!(trace, "web-services-test");
        let response =
            arcbox_connect::v1::ListGuestWebServicesResponse::decode_from_slice(&payload).unwrap();
        assert_eq!(response.services.len(), 1);
        let Some(arcbox_connect::v1::guest_web_service::Route::Container(service)) =
            response.services[0].route.as_ref()
        else {
            panic!("expected container route")
        };
        assert_eq!(service.container_id, "a");
        assert_eq!(service.hostnames, ["api.arcbox.local"]);
        assert_eq!(service.ipv4_address, "172.17.0.2");
        assert_eq!(service.http_port, 0);
        assert_eq!(
            service.https_backend,
            Some(arcbox_connect::v1::container_web_service::HttpsBackend::NativeHttpsPort(8443))
        );
        let exact = |owner: &str| {
            arcbox_dns::claims::Claim::exclusive(
                arcbox_dns::NamePattern::Exact("api.svc.local".parse().unwrap()),
                owner.into(),
                "container".into(),
            )
        };
        for (change, hostname, owner) in [
            ("wildcard", "API.SVC.LOCAL", Some("a")),
            ("nested", "v1.api.svc.local", Some("a")),
            ("apex", "svc.local", None),
            ("invalid", "*.svc.local", None),
            ("exact", "api.svc.local", Some("b")),
            ("conflict", "api.svc.local", None),
            ("exact-unavailable", "api.svc.local", Some("a")),
            ("recovered", "api.svc.local", Some("a")),
            ("addressless", "api.svc.local", None),
            ("stale", "api.svc.local", None),
        ] {
            {
                let registry = dns.registry();
                let mut registry = registry.write().unwrap();
                match change {
                    "exact" | "conflict" => {
                        let id = if change == "exact" { "b" } else { "c" };
                        registry
                            .replace(
                                id.into(),
                                vec![exact(id)],
                                Some("172.17.0.2".parse().unwrap()),
                            )
                            .unwrap();
                    }
                    "exact-unavailable" => {
                        registry.remove("c");
                        registry
                            .replace("b".into(), vec![exact("b")], None)
                            .unwrap();
                    }
                    "recovered" => {
                        registry.remove("b");
                    }
                    "addressless" | "stale" => {
                        let claims = registry.get("a").unwrap().claims.clone();
                        let address = (change == "stale").then(|| "172.17.0.3".parse().unwrap());
                        registry.replace("a".into(), claims, address).unwrap();
                    }
                    _ => {}
                }
            }
            let request = arcbox_connect::v1::ListGuestWebServicesRequest {
                hostname: hostname.into(),
                ..Default::default()
            };
            write_message(
                &mut client,
                crate::rpc::MessageType::ListGuestWebServicesRequest,
                change,
                &request.encode_to_vec(),
            )
            .await
            .unwrap();
            let (kind, trace, payload) = read_message(&mut client).await.unwrap();
            assert_eq!(trace, change);
            if change == "invalid" {
                assert_eq!(kind, crate::rpc::MessageType::Error);
                assert_eq!(ErrorResponse::decode(&payload).unwrap().code, 400);
                continue;
            }
            assert_eq!(kind, crate::rpc::MessageType::ListGuestWebServicesResponse);
            let response =
                arcbox_connect::v1::ListGuestWebServicesResponse::decode_from_slice(&payload)
                    .unwrap();
            assert_eq!(
                response.services.len(),
                usize::from(owner.is_some()),
                "{change}"
            );
            if let Some(owner) = owner {
                let Some(arcbox_connect::v1::guest_web_service::Route::Container(route)) =
                    response.services[0].route.as_ref()
                else {
                    panic!("expected container route")
                };
                assert_eq!(route.container_id, owner);
                assert_eq!(route.hostnames, [hostname.to_ascii_lowercase()]);
                assert_eq!(route.https_backend, service.https_backend);
            }
        }
        drop(client);
        task.await.unwrap().unwrap();
    }
}
