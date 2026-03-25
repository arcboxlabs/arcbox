//! Smart proxy for forwarding Docker API requests to guest dockerd.
//!
//! Provides HTTP/1.1 client over vsock to forward requests, with support
//! for streaming responses and HTTP upgrades (attach, exec).

use crate::api::AppState;
use crate::error::{DockerError, Result};
use arcbox_core::Runtime;
use arcbox_error::CommonError;
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, header};
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::Semaphore;

// =============================================================================
// GuestConnector — abstraction over guest connection establishment
// =============================================================================

/// Abstraction over guest connection establishment.
///
/// Production code connects via vsock ([`VsockConnector`]); integration tests
/// can connect via Unix socket. Both produce a [`TokioIo<RawFdStream>`]
/// because [`RawFdStream`] wraps any pollable file descriptor.
pub trait GuestConnector: Send + Sync + 'static {
    /// Opens a new connection to guest dockerd.
    fn connect(&self) -> Pin<Box<dyn Future<Output = Result<TokioIo<RawFdStream>>> + Send + '_>>;
}

/// Timeout for establishing a vsock connection to guest dockerd.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for the HTTP/1.1 handshake with guest dockerd.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of concurrent vsock connection attempts.
///
/// Prevents blocking thread pool exhaustion when the guest is unresponsive
/// and Docker clients retry aggressively.
const MAX_CONCURRENT_CONNECTS: usize = 8;

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

pub struct RawFdStream {
    inner: AsyncFd<RawFdWrapper>,
}

impl RawFdStream {
    /// Creates a new async stream from an owned file descriptor.
    ///
    /// The fd is set to non-blocking mode. Ownership is taken via
    /// [`OwnedFd`] so the descriptor is always closed on error.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        Self::set_nonblocking(fd.as_raw_fd())?;
        let inner = AsyncFd::new(RawFdWrapper(fd))?;
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
        // macOS vsock does not support half-close: `shutdown(fd, SHUT_WR)`
        // destroys the entire connection instead of closing only the write
        // side. This breaks hyper's upgrade path because the fd becomes
        // ENOTCONN before the bridge can start. The fd is properly closed
        // when `OwnedFd` is dropped, so the no-op is safe.
        #[cfg(target_os = "macos")]
        {
            return Poll::Ready(Ok(()));
        }

        #[cfg(not(target_os = "macos"))]
        {
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
}

// =============================================================================
// VsockConnector — production guest connection via vsock
// =============================================================================

/// Connects to guest dockerd via vsock through the machine manager.
pub struct VsockConnector {
    runtime: Arc<Runtime>,
}

impl VsockConnector {
    #[must_use]
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

impl GuestConnector for VsockConnector {
    fn connect(&self) -> Pin<Box<dyn Future<Output = Result<TokioIo<RawFdStream>>> + Send + '_>> {
        Box::pin(async move {
            let _permit = CONNECT_SEMAPHORE
                .acquire()
                .await
                .map_err(|_| DockerError::Server("connect semaphore closed".into()))?;

            let port = self.runtime.guest_docker_vsock_port();
            let machine_name = self.runtime.default_machine_name();
            let manager = self.runtime.machine_manager().clone();
            let name = machine_name.to_string();

            let handle = tokio::task::spawn_blocking(move || {
                let fd = manager.connect_vsock_port(&name, port)?;
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
                    .map_err(|e| {
                        DockerError::Server(format!("failed to connect to guest docker: {e}"))
                    })?,
                Err(_elapsed) => {
                    abort_handle.abort();
                    return Err(CommonError::timeout("guest docker connect timed out").into());
                }
            };

            let stream = RawFdStream::new(owned_fd)
                .map_err(|e| DockerError::Server(format!("failed to create guest stream: {e}")))?;
            Ok(TokioIo::new(stream))
        })
    }
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
    connector: &dyn GuestConnector,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>> {
    let io = connector.connect().await?;

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

    // Forward content-type so the guest dockerd can parse JSON bodies.
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req.headers_mut().insert(header::CONTENT_TYPE, ct.clone());
    }
    req.headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));
    req.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));

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
    connector: &dyn GuestConnector,
    original_uri: &Uri,
    req: Request<Body>,
) -> Result<Response<Body>> {
    let io = connector.connect().await?;

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
    let content_type = req.headers().get(header::CONTENT_TYPE).cloned();
    let body = req.into_body();

    let mut guest_req = hyper::Request::builder()
        .method(method)
        .uri(path_and_query)
        .body(body)
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    if let Some(ct) = content_type {
        guest_req.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    guest_req
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));
    guest_req
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));

    let response = sender
        .send_request(guest_req)
        .await
        .map_err(|e| DockerError::Server(format!("guest docker request failed: {e}")))?;

    let (parts, incoming) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

/// Send a raw HTTP/1.1 upgrade request to guest dockerd and read the
/// response headers.
///
/// Bypasses hyper's client-side upgrade API. hyper's `upgrade::on(response)`
/// transfers the IO through an internal oneshot channel, but the upgraded
/// future never resolves for `TokioIo<RawFdStream>` — the IO is silently
/// lost. Writing the HTTP exchange directly keeps the vsock fd owned by
/// the caller throughout the entire upgrade + bridge lifecycle.
async fn send_raw_upgrade(
    stream: &mut RawFdStream,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(StatusCode, HeaderMap)> {
    // Build HTTP/1.1 request.
    let mut raw = format!("{method} {path_and_query} HTTP/1.1\r\nHost: localhost\r\n");
    let has_content_length = headers.contains_key(header::CONTENT_LENGTH);
    let has_transfer_encoding = headers.contains_key(header::TRANSFER_ENCODING);
    for (key, value) in headers {
        if key == header::HOST {
            continue;
        }
        // Override Content-Length to match the body we actually send.
        if key == header::CONTENT_LENGTH {
            continue;
        }
        let Ok(v) = value.to_str() else { continue };
        raw.push_str(key.as_str());
        raw.push_str(": ");
        raw.push_str(v);
        raw.push_str("\r\n");
    }
    // Always emit an accurate Content-Length so guest dockerd does not
    // wait for bytes that will never arrive.
    if !has_transfer_encoding {
        raw.push_str(&format!("content-length: {}\r\n", body.len()));
    } else if !has_content_length && !has_transfer_encoding {
        raw.push_str("content-length: 0\r\n");
    }
    raw.push_str("\r\n");

    stream
        .write_all(raw.as_bytes())
        .await
        .map_err(|e| DockerError::Server(format!("failed to write upgrade request: {e}")))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .await
            .map_err(|e| DockerError::Server(format!("failed to write upgrade body: {e}")))?;
    }

    // Read response headers byte-by-byte until the blank line delimiter.
    // Upgrade responses are small (< 512 bytes), so this is fine.
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|e| DockerError::Server(format!("failed to read upgrade response: {e}")))?;
        buf.push(byte[0]);
        if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
        if buf.len() > 8192 {
            return Err(DockerError::Server(
                "upgrade response headers too large".into(),
            ));
        }
    }

    // Parse "HTTP/1.1 101 Switching Protocols\r\n..."
    let header_str = String::from_utf8_lossy(&buf);
    let status_line = header_str
        .lines()
        .next()
        .ok_or_else(|| DockerError::Server("empty upgrade response".into()))?;
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| DockerError::Server(format!("invalid status line: {status_line}")))?;
    let status = StatusCode::from_u16(status_code)
        .map_err(|_| DockerError::Server(format!("invalid status code: {status_code}")))?;

    let mut response_headers = HeaderMap::new();
    for line in header_str.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if let (Ok(name), Ok(val)) = (
                header::HeaderName::from_bytes(key.trim().as_bytes()),
                header::HeaderValue::from_str(value.trim()),
            ) {
                response_headers.insert(name, val);
            }
        }
    }

    Ok((status, response_headers))
}

/// Forward an HTTP request with upgrade support to guest dockerd.
///
/// Used for attach/exec (raw-stream) and BuildKit gRPC/session (h2c)
/// endpoints. After the 101 handshake, client and guest streams are
/// bridged via `copy_bidirectional`.
///
/// The guest side uses a raw HTTP exchange instead of hyper's
/// `upgrade::on()` API. hyper's client-side upgrade transfers the IO
/// through an internal oneshot channel that never delivers for
/// `TokioIo<RawFdStream>`, leaving the bridge future permanently
/// blocked. Writing the HTTP exchange directly keeps the vsock fd
/// alive and owned by the caller for the entire bridge lifetime.
///
/// # Errors
///
/// Returns an error if guest connection, upgrade handshake, or response
/// construction fails.
pub async fn proxy_with_upgrade(
    connector: &dyn GuestConnector,
    mut client_req: axum::http::Request<Body>,
    original_uri: &Uri,
) -> Result<Response<Body>> {
    let io = connector.connect().await?;
    // Unwrap TokioIo to get the raw vsock stream — we drive the guest
    // side manually so the fd stays alive throughout the bridge.
    let mut guest_stream = io.into_inner();

    let path_and_query = original_uri
        .path_and_query()
        .map_or("/", hyper::http::uri::PathAndQuery::as_str);

    // Collect the request body so it can be forwarded to the guest.
    // Upgrade request bodies (e.g. exec-start JSON) are small.
    let body_bytes = {
        let body = std::mem::take(client_req.body_mut());
        http_body_util::BodyExt::collect(body)
            .await
            .map_err(|e| DockerError::Server(format!("failed to read request body: {e}")))?
            .to_bytes()
    };

    let (status, response_headers) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        send_raw_upgrade(
            &mut guest_stream,
            client_req.method(),
            path_and_query,
            client_req.headers(),
            &body_bytes,
        ),
    )
    .await
    .map_err(|_| DockerError::from(CommonError::timeout("guest docker upgrade timed out")))??;

    if status != StatusCode::SWITCHING_PROTOCOLS {
        // Forward the guest's actual error status and headers so the
        // client sees actionable failures instead of a generic 500.
        let mut builder = Response::builder().status(status);
        for (key, value) in &response_headers {
            builder = builder.header(key, value);
        }
        // Read whatever response body the guest sent (bounded).
        let mut error_body = vec![0u8; 8192];
        let n = guest_stream.read(&mut error_body).await.unwrap_or(0);
        error_body.truncate(n);
        return builder
            .body(Body::from(error_body))
            .map_err(|e| DockerError::Server(format!("failed to build error response: {e}")));
    }

    // Forward the guest's actual Upgrade value (h2c, tcp, etc.)
    // so the client negotiates the correct post-upgrade protocol.
    let upgrade_proto = response_headers
        .get(header::UPGRADE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("tcp"));
    let content_type = response_headers.get(header::CONTENT_TYPE).cloned();

    // Ensure no leftover request body data interferes with the upgraded stream.
    *client_req.body_mut() = Body::empty();
    let client_upgrade = hyper::upgrade::on(&mut client_req);

    let mut builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, upgrade_proto);

    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    let response = builder
        .body(Body::empty())
        .map_err(|e| DockerError::Server(format!("failed to build upgrade response: {e}")))?;

    // Bridge upgraded connections in background.
    tokio::spawn(async move {
        let client_io = match client_upgrade.await {
            Ok(io) => io,
            Err(e) => {
                tracing::debug!("upgrade bridging setup failed: {}", e);
                return;
            }
        };
        let mut client_io = TokioIo::new(client_io);
        if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut guest_stream).await {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("broken pipe")
                && !msg.contains("connection reset")
                && !msg.contains("not connected")
            {
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
        return proxy_with_upgrade(state.connector.as_ref(), req, &uri).await;
    }

    proxy_to_guest_stream(state.connector.as_ref(), &uri, req).await
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
}
