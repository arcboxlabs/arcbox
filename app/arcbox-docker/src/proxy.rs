//! Smart proxy for forwarding Docker API requests to guest dockerd.
//!
//! Provides HTTP/1.1 client over vsock to forward requests, with support
//! for streaming responses and HTTP upgrades (attach, exec).

use crate::api::AppState;
use crate::error::{DockerError, Result};
use arcbox_core::Runtime;
use arcbox_error::CommonError;
use axum::body::{Body, BodyDataStream};
use axum::extract::{OriginalUri, State};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header,
};
use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::Full;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// Timeout for establishing a vsock connection to guest dockerd.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for the HTTP/1.1 handshake with guest dockerd.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of concurrent vsock connection attempts.
///
/// Prevents blocking thread pool exhaustion when the guest is unresponsive
/// and Docker clients retry aggressively.
const MAX_CONCURRENT_CONNECTS: usize = 8;
/// Buffer size for upload body chunks proxied to guest dockerd.
const UPLOAD_BODY_BUFFER: usize = 16;

static CONNECT_SEMAPHORE: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_CONNECTS));

// =============================================================================
// RawFdStream — async I/O wrapper around a raw vsock file descriptor
// =============================================================================

struct RawFdWrapper(OwnedFd);

impl AsRawFd for RawFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub(crate) struct RawFdStream {
    inner: AsyncFd<RawFdWrapper>,
}

impl RawFdStream {
    pub(crate) fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid file descriptor",
            ));
        }

        Self::set_nonblocking(fd)?;
        // SAFETY: fd is a valid, newly-opened vsock file descriptor owned by us.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let inner = AsyncFd::new(RawFdWrapper(owned))?;
        Ok(Self { inner })
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: F_GETFL/F_SETFL are safe on valid fds.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsyncRead for RawFdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let slice = buf.initialize_unfilled();
                // SAFETY: reading into our buffer from a valid fd.
                let n = unsafe { libc::read(fd, slice.as_mut_ptr().cast(), slice.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // n >= 0 is enforced above.
                    #[allow(clippy::cast_sign_loss)]
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for RawFdStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                // SAFETY: writing from our buffer to a valid fd.
                let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // n >= 0 is enforced above.
                    #[allow(clippy::cast_sign_loss)]
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = self.inner.get_ref().as_raw_fd();
        // SAFETY: shutdown on a valid fd.
        let result = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
        if result == 0 {
            return Poll::Ready(Ok(()));
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOTCONN) {
            return Poll::Ready(Ok(()));
        }
        Poll::Ready(Err(err))
    }
}

// =============================================================================
// Guest connection
// =============================================================================

/// Opens a vsock connection to guest dockerd with a timeout.
///
/// A semaphore limits concurrent connection attempts to prevent blocking
/// thread pool exhaustion when the guest is unresponsive. If the timeout
/// fires, the spawned blocking task may still complete — we close the
/// leaked fd in that case to avoid resource leaks.
async fn connect_guest(runtime: &Runtime) -> Result<TokioIo<RawFdStream>> {
    let _permit = CONNECT_SEMAPHORE
        .acquire()
        .await
        .map_err(|_| DockerError::Server("connect semaphore closed".into()))?;

    let port = runtime.guest_docker_vsock_port();
    let machine_name = runtime.default_machine_name();
    let manager = runtime.machine_manager().clone();
    let name = machine_name.to_string();

    let handle = tokio::task::spawn_blocking(move || {
        let fd = manager.connect_vsock_port(&name, port)?;
        // Wrap in OwnedFd so the fd is closed on drop if the result is
        // discarded (e.g. after a timeout abort).
        // SAFETY: fd is a valid, newly-opened vsock file descriptor.
        Ok::<_, arcbox_core::CoreError>(unsafe { OwnedFd::from_raw_fd(fd) })
    });
    let abort_handle = handle.abort_handle();

    let owned_fd = match tokio::time::timeout(CONNECT_TIMEOUT, handle).await {
        Ok(join_result) => join_result
            .map_err(|e| {
                let reason = if e.is_cancelled() {
                    "cancelled"
                } else {
                    "panicked"
                };
                DockerError::Server(format!("connect task {reason}: {e}"))
            })?
            .map_err(|e| DockerError::Server(format!("failed to connect to guest docker: {e}")))?,
        Err(_elapsed) => {
            // Abort the join handle. The blocking task may still complete,
            // but the OwnedFd in its return value will be dropped (closed)
            // when tokio discards the aborted task's output.
            abort_handle.abort();
            return Err(CommonError::timeout("guest docker connect timed out").into());
        }
    };

    let stream = RawFdStream::from_raw_fd(owned_fd.into_raw_fd())
        .map_err(|e| DockerError::Server(format!("failed to create guest stream: {e}")))?;
    Ok(TokioIo::new(stream))
}

// =============================================================================
// Proxy helpers
// =============================================================================

/// Forward an HTTP request to guest dockerd and return the response.
///
/// Opens a new HTTP/1.1 connection over vsock for each request. The response
/// body is streamed lazily, so this works for both fixed-length and chunked
/// (streaming) responses like logs and events.
///
/// # Errors
///
/// Returns an error if guest connection, handshake, request forwarding,
/// or response mapping fails.
pub async fn proxy_to_guest(
    runtime: &Runtime,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>> {
    let io = connect_guest(runtime).await?;

    let (mut sender, conn) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, http1::Builder::new().handshake(io))
            .await
            .map_err(|_| {
                DockerError::from(CommonError::timeout("guest docker handshake timed out"))
            })?
            .map_err(|e| DockerError::Server(format!("guest docker handshake failed: {e}")))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("canceled") && !msg.contains("incomplete") {
                tracing::debug!("guest docker connection ended: {}", e);
            }
        }
    });

    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path_and_query)
        .body(Full::new(body))
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    req.headers_mut()
        .extend(forwarded_request_headers(headers, false));
    req.headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));

    let response = sender
        .send_request(req)
        .await
        .map_err(|e| DockerError::Server(format!("guest docker request failed: {e}")))?;

    let (parts, incoming) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

/// Forward an HTTP request to guest dockerd without buffering the request body.
///
/// This is used by pass-through proxy paths so large uploads and streamed
/// payloads are relayed directly instead of being collected in memory.
///
/// # Errors
///
/// Returns an error if guest connection, handshake, request forwarding,
/// or response mapping fails.
pub async fn proxy_to_guest_stream(
    runtime: &Runtime,
    original_uri: &Uri,
    req: Request<Body>,
) -> Result<Response<Body>> {
    let io = connect_guest(runtime).await?;
    proxy_to_guest_stream_with_io(io, original_uri, req).await
}

/// Forward a large upload request to guest dockerd.
///
/// This path is used for Docker endpoints like `POST /images/load` where the
/// proxy must own `Expect` handling and request-body draining semantics.
pub async fn proxy_streaming_upload(
    runtime: &Runtime,
    original_uri: &Uri,
    req: Request<Body>,
) -> Result<Response<Body>> {
    let io = connect_guest(runtime).await?;
    proxy_streaming_upload_with_io(io, original_uri, req).await
}

async fn proxy_to_guest_stream_with_io<T>(
    io: TokioIo<T>,
    original_uri: &Uri,
    req: Request<Body>,
) -> Result<Response<Body>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tracing::debug!(
        method = %req.method(),
        uri = %original_uri,
        "proxying streamed Docker request to guest"
    );

    let (mut sender, conn) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, http1::Builder::new().handshake(io))
            .await
            .map_err(|_| {
                DockerError::from(CommonError::timeout("guest docker handshake timed out"))
            })?
            .map_err(|e| DockerError::Server(format!("guest docker handshake failed: {e}")))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("canceled") && !msg.contains("incomplete") {
                tracing::debug!("guest docker connection ended: {}", e);
            }
        }
    });

    let path_and_query = original_uri
        .path_and_query()
        .map_or("/", hyper::http::uri::PathAndQuery::as_str);
    let method = req.method().clone();
    let forwarded_headers = forwarded_request_headers(req.headers(), false);
    let body = req.into_body();

    let mut guest_req = hyper::Request::builder()
        .method(method)
        .uri(path_and_query)
        .body(body)
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    guest_req.headers_mut().extend(forwarded_headers);
    guest_req
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));

    let response = sender
        .send_request(guest_req)
        .await
        .map_err(|e| DockerError::Server(format!("guest docker request failed: {e}")))?;

    let (parts, incoming) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

async fn proxy_streaming_upload_with_io<T>(
    io: TokioIo<T>,
    original_uri: &Uri,
    req: Request<Body>,
) -> Result<Response<Body>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let had_expect = req.headers().contains_key(header::EXPECT);
    let had_content_length = req.headers().contains_key(header::CONTENT_LENGTH);
    tracing::debug!(
        method = %req.method(),
        uri = %original_uri,
        had_expect,
        had_content_length,
        "proxying Docker upload request to guest"
    );

    let (mut sender, conn) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, http1::Builder::new().handshake(io))
            .await
            .map_err(|_| {
                DockerError::from(CommonError::timeout("guest docker handshake timed out"))
            })?
            .map_err(|e| DockerError::Server(format!("guest docker handshake failed: {e}")))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("canceled") && !msg.contains("incomplete") {
                tracing::debug!("guest docker connection ended: {}", e);
            }
        }
    });

    let path_and_query = original_uri
        .path_and_query()
        .map_or("/", hyper::http::uri::PathAndQuery::as_str);
    let method = req.method().clone();
    let forwarded_headers = forwarded_request_headers(req.headers(), true);
    let body_stream = req.into_body().into_data_stream();
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, io::Error>>(UPLOAD_BODY_BUFFER);
    let body_pump = tokio::spawn(async move { pump_upload_body(body_stream, tx).await });
    let guest_body = Body::from_stream(ReceiverStream::new(rx));

    let mut guest_req = hyper::Request::builder()
        .method(method)
        .uri(path_and_query)
        .body(guest_body)
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    guest_req.headers_mut().extend(forwarded_headers);
    guest_req
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));

    let response = match sender.send_request(guest_req).await {
        Ok(response) => response,
        Err(e) => {
            let drain_result = join_upload_pump(body_pump).await;
            tracing::debug!(
                drain_ok = drain_result.is_ok(),
                "guest upload request failed before response headers"
            );
            return Err(DockerError::Server(format!(
                "guest docker upload request failed: {e}"
            )));
        }
    };

    let uploaded_bytes = join_upload_pump(body_pump).await?;
    tracing::debug!(uploaded_bytes, "completed Docker upload body pump");

    let (parts, incoming) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

/// Forward an HTTP request with upgrade support to guest dockerd.
///
/// Used for attach and exec endpoints that use HTTP upgrade (101 Switching
/// Protocols) for bidirectional streaming. After the upgrade, client and
/// guest streams are bridged via `copy_bidirectional`.
///
/// # Errors
///
/// Returns an error if guest connection, handshake, request forwarding,
/// or response construction fails.
pub async fn proxy_with_upgrade(
    runtime: &Runtime,
    mut client_req: axum::http::Request<Body>,
    original_uri: &Uri,
) -> Result<Response<Body>> {
    let io = connect_guest(runtime).await?;

    let (mut sender, conn) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, http1::Builder::new().handshake(io))
            .await
            .map_err(|_| {
                DockerError::from(CommonError::timeout("guest docker handshake timed out"))
            })?
            .map_err(|e| DockerError::Server(format!("guest docker handshake failed: {e}")))?;

    // The connection task must keep running for the upgrade to work.
    // `.with_upgrades()` is required so hyper::upgrade::on(response) works.
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("canceled") && !msg.contains("incomplete") {
                tracing::debug!("guest docker upgrade connection ended: {}", e);
            }
        }
    });

    let path_and_query = original_uri
        .path_and_query()
        .map_or("/", hyper::http::uri::PathAndQuery::as_str);
    let req_body = std::mem::take(client_req.body_mut());

    let mut guest_req = hyper::Request::builder()
        .method(client_req.method())
        .uri(path_and_query)
        .body(req_body)
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    // Forward all headers except Host.
    for (key, value) in client_req.headers() {
        if key != header::HOST {
            guest_req.headers_mut().insert(key.clone(), value.clone());
        }
    }
    guest_req
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));

    let guest_response = sender
        .send_request(guest_req)
        .await
        .map_err(|e| DockerError::Server(format!("guest docker request failed: {e}")))?;

    if guest_response.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Guest didn't upgrade — return its response as-is.
        let (parts, incoming) = guest_response.into_parts();
        return Ok(Response::from_parts(parts, Body::new(incoming)));
    }

    // Preserve content-type from guest's 101 (raw-stream vs multiplexed-stream).
    let content_type = guest_response.headers().get(header::CONTENT_TYPE).cloned();

    // Prepare both upgrade futures BEFORE returning the 101 to the client.
    let client_upgrade = hyper::upgrade::on(&mut client_req);
    let guest_upgrade = hyper::upgrade::on(guest_response);

    let mut builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "tcp");

    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    let response = builder
        .body(Body::empty())
        .map_err(|e| DockerError::Server(format!("failed to build upgrade response: {e}")))?;

    // Bridge upgraded connections in background.
    tokio::spawn(async move {
        let (client_io, guest_io) = match tokio::try_join!(client_upgrade, guest_upgrade) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("upgrade bridging setup failed: {}", e);
                return;
            }
        };
        let mut client_io = TokioIo::new(client_io);
        let mut guest_io = TokioIo::new(guest_io);
        if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut guest_io).await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("broken pipe") && !msg.contains("connection reset") {
                tracing::debug!("upgrade bridge error: {}", e);
            }
        }
    });

    Ok(response)
}

// =============================================================================
// Fallback handler — catches any unmatched route and proxies to guest
// =============================================================================

/// Catch-all handler that proxies unmatched requests to guest dockerd.
///
/// Ensures forward compatibility with newer Docker API versions — any endpoint
/// we don't explicitly handle gets forwarded transparently.
///
/// # Errors
///
/// Returns an error if VM readiness fails or guest proxying fails.
pub async fn proxy_fallback(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    req: axum::http::Request<Body>,
) -> Result<Response<Body>> {
    tracing::debug!("proxy_fallback: method={} uri={}", req.method(), uri);
    state
        .runtime
        .ensure_vm_ready()
        .await
        .map_err(|e| DockerError::Server(format!("failed to ensure VM is ready: {e}")))?;

    // Detect upgrade requests (attach, exec).
    let wants_upgrade = req.headers().get(header::UPGRADE).is_some()
        || req
            .headers()
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));

    if wants_upgrade {
        return proxy_with_upgrade(&state.runtime, req, &uri).await;
    }

    if is_streaming_upload_request(req.method(), &uri) {
        return proxy_streaming_upload(&state.runtime, &uri, req).await;
    }

    proxy_to_guest_stream(&state.runtime, &uri, req).await
}

fn forwarded_request_headers(headers: &HeaderMap, strip_expect: bool) -> HeaderMap {
    let connection_tokens = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| token.trim().parse::<HeaderName>().ok())
        .collect::<Vec<_>>();

    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if name == header::HOST
            || name == header::CONNECTION
            || (strip_expect && name == header::EXPECT)
            || is_hop_by_hop_header(name)
            || connection_tokens.iter().any(|token| token == name)
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_streaming_upload_request(method: &Method, uri: &Uri) -> bool {
    if *method != Method::POST {
        return false;
    }

    let path = uri.path();
    path == "/images/load" || path.ends_with("/images/load")
}

async fn pump_upload_body(
    mut body_stream: BodyDataStream,
    tx: mpsc::Sender<std::result::Result<Bytes, io::Error>>,
) -> Result<u64> {
    let mut uploaded_bytes = 0_u64;
    let mut receiver_dropped = false;

    while let Some(chunk) = body_stream.next().await {
        match chunk {
            Ok(bytes) => {
                uploaded_bytes = uploaded_bytes.saturating_add(bytes.len() as u64);
                if receiver_dropped {
                    continue;
                }

                if tx.send(Ok(bytes)).await.is_err() {
                    receiver_dropped = true;
                    tracing::debug!("guest upload body receiver closed, draining client upload");
                }
            }
            Err(error) => {
                let io_error = io::Error::other(error.to_string());
                if !receiver_dropped {
                    let _ = tx.send(Err(io_error)).await;
                }
                return Err(DockerError::Server(format!(
                    "failed to read client upload body: {error}"
                )));
            }
        }
    }

    drop(tx);

    if receiver_dropped {
        Err(DockerError::Server(
            "guest upload body receiver closed before request completed".into(),
        ))
    } else {
        Ok(uploaded_bytes)
    }
}

async fn join_upload_pump(handle: tokio::task::JoinHandle<Result<u64>>) -> Result<u64> {
    handle
        .await
        .map_err(|e| DockerError::Server(format!("upload body pump task failed: {e}")))?
}

// =============================================================================
// Port binding parsing (for post-hook port forwarding)
// =============================================================================

/// A parsed port binding from Docker inspect JSON.
#[derive(Debug, Clone)]
pub struct PortBindingInfo {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: String,
}

/// Parse port bindings from a Docker container inspect JSON response.
///
/// Extracts `NetworkSettings.Ports` and `HostConfig.PortBindings` into a flat
/// list of `PortBindingInfo` structs suitable for port forwarding setup.
#[must_use]
pub fn parse_port_bindings(inspect_json: &[u8]) -> Vec<PortBindingInfo> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(inspect_json) else {
        return vec![];
    };

    // Prefer NetworkSettings.Ports (reflects runtime state).
    let ports = value
        .pointer("/NetworkSettings/Ports")
        .or_else(|| value.pointer("/HostConfig/PortBindings"));

    let Some(ports) = ports.and_then(|v| v.as_object()) else {
        return vec![];
    };

    let mut bindings = Vec::new();

    for (container_port_proto, host_bindings) in ports {
        // Parse "80/tcp" or "53/udp"
        let (container_port, protocol) =
            if let Some((port_str, proto)) = container_port_proto.split_once('/') {
                let port: u16 = match port_str.parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                (port, proto.to_string())
            } else {
                let port: u16 = match container_port_proto.parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                (port, "tcp".to_string())
            };

        let Some(bindings_arr) = host_bindings.as_array() else {
            continue;
        };

        for binding in bindings_arr {
            let host_ip = binding
                .get("HostIp")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let host_port: u16 = binding
                .get("HostPort")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            if host_port > 0 {
                bindings.push(PortBindingInfo {
                    host_ip,
                    host_port,
                    container_port,
                    protocol: protocol.clone(),
                });
            }
        }
    }

    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use hyper::body::Incoming;
    use hyper::server::conn::http1 as server_http1;
    use hyper_util::rt::TokioIo;
    use serde_json::Value;
    use tokio::net::TcpListener;

    #[test]
    fn parse_port_bindings_from_network_settings() {
        let json = serde_json::json!({
            "NetworkSettings": {
                "Ports": {
                    "80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "8080"}],
                    "443/tcp": [{"HostIp": "", "HostPort": "8443"}]
                }
            }
        });
        let bindings = parse_port_bindings(serde_json::to_vec(&json).unwrap().as_slice());
        assert_eq!(bindings.len(), 2);

        let b80 = bindings.iter().find(|b| b.container_port == 80).unwrap();
        assert_eq!(b80.host_port, 8080);
        assert_eq!(b80.protocol, "tcp");
        assert_eq!(b80.host_ip, "0.0.0.0");

        let b443 = bindings.iter().find(|b| b.container_port == 443).unwrap();
        assert_eq!(b443.host_port, 8443);
    }

    #[test]
    fn parse_port_bindings_empty_ports() {
        let json = serde_json::json!({"NetworkSettings": {"Ports": {}}});
        let bindings = parse_port_bindings(serde_json::to_vec(&json).unwrap().as_slice());
        assert!(bindings.is_empty());
    }

    #[test]
    fn parse_port_bindings_null_host_bindings() {
        let json = serde_json::json!({
            "NetworkSettings": {
                "Ports": {
                    "80/tcp": null
                }
            }
        });
        let bindings = parse_port_bindings(serde_json::to_vec(&json).unwrap().as_slice());
        assert!(bindings.is_empty());
    }

    #[test]
    fn parse_port_bindings_udp() {
        let json = serde_json::json!({
            "NetworkSettings": {
                "Ports": {
                    "53/udp": [{"HostIp": "", "HostPort": "5353"}]
                }
            }
        });
        let bindings = parse_port_bindings(serde_json::to_vec(&json).unwrap().as_slice());
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].protocol, "udp");
        assert_eq!(bindings[0].container_port, 53);
        assert_eq!(bindings[0].host_port, 5353);
    }

    #[test]
    fn forwarded_headers_preserve_end_to_end_fields() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-tar"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("4"));
        headers.insert(header::EXPECT, HeaderValue::from_static("100-continue"));
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-custom-hop"),
        );
        headers.insert(
            HeaderName::from_static("x-custom-hop"),
            HeaderValue::from_static("secret"),
        );
        headers.insert(
            HeaderName::from_static("x-forward-me"),
            HeaderValue::from_static("yes"),
        );

        let forwarded = forwarded_request_headers(&headers, false);
        assert_eq!(
            forwarded.get(header::CONTENT_TYPE).unwrap(),
            "application/x-tar"
        );
        assert_eq!(forwarded.get(header::CONTENT_LENGTH).unwrap(), "4");
        assert_eq!(forwarded.get(header::EXPECT).unwrap(), "100-continue");
        assert_eq!(forwarded.get("x-forward-me").unwrap(), "yes");
        assert!(forwarded.get(header::CONNECTION).is_none());
        assert!(forwarded.get("x-custom-hop").is_none());
    }

    #[test]
    fn forwarded_upload_headers_strip_expect() {
        let mut headers = HeaderMap::new();
        headers.insert(header::EXPECT, HeaderValue::from_static("100-continue"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-tar"),
        );

        let forwarded = forwarded_request_headers(&headers, true);
        assert!(forwarded.get(header::EXPECT).is_none());
        assert_eq!(
            forwarded.get(header::CONTENT_TYPE).unwrap(),
            "application/x-tar"
        );
    }

    #[tokio::test]
    async fn proxy_streaming_upload_forwards_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service =
                hyper::service::service_fn(|request: hyper::Request<Incoming>| async move {
                    let headers = request
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.as_str().to_string(),
                                value.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect::<std::collections::HashMap<_, _>>();
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let payload = serde_json::json!({
                        "headers": headers,
                        "body": String::from_utf8_lossy(&body),
                    });
                    let response = hyper::Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::from(payload.to_string())))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(response)
                });

            server_http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
                .unwrap();
        });

        let io = TokioIo::new(tokio::net::TcpStream::connect(addr).await.unwrap());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/images/load?quiet=1")
            .header(header::CONTENT_TYPE, "application/x-tar")
            .header(header::CONTENT_LENGTH, "4")
            .header(header::EXPECT, "100-continue")
            .header(header::CONNECTION, "keep-alive, x-custom-hop")
            .header("x-custom-hop", "drop-me")
            .header("x-forward-me", "yes")
            .body(Body::from("data"))
            .unwrap();
        let original_uri = Uri::from_static("/v1.47/images/load?quiet=1");

        let response = proxy_streaming_upload_with_io(io, &original_uri, request)
            .await
            .unwrap();
        let payload = response.into_body().collect().await.unwrap().to_bytes();
        let payload: Value = serde_json::from_slice(&payload).unwrap();
        let headers = payload.get("headers").unwrap();
        assert_eq!(headers.get("host").unwrap(), "localhost");
        assert_eq!(headers.get("content-type").unwrap(), "application/x-tar");
        assert_eq!(headers.get("content-length").unwrap(), "4");
        assert!(headers.get("expect").is_none());
        assert_eq!(headers.get("x-forward-me").unwrap(), "yes");
        assert!(headers.get("connection").is_none());
        assert!(headers.get("x-custom-hop").is_none());
        assert_eq!(payload.get("body").unwrap(), "data");

        server.await.unwrap();
    }

    #[test]
    fn detects_image_load_upload_requests() {
        assert!(is_streaming_upload_request(
            &Method::POST,
            &Uri::from_static("/v1.47/images/load?quiet=1")
        ));
        assert!(!is_streaming_upload_request(
            &Method::GET,
            &Uri::from_static("/v1.47/images/load?quiet=1")
        ));
        assert!(!is_streaming_upload_request(
            &Method::POST,
            &Uri::from_static("/v1.47/images/json")
        ));
    }
}
