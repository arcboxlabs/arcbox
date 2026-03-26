//! Hyper-based request forwarding to guest dockerd.
//!
//! Standard (non-upgrade) proxy paths that use hyper's HTTP/1.1 client to
//! relay requests and stream responses back to the Docker CLI.

use super::{GuestConnector, HANDSHAKE_TIMEOUT};
use crate::error::{DockerError, Result};
use arcbox_error::CommonError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, Uri, header};
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http1;

/// Forwards all non-hop-by-hop headers from the client request to the guest request.
///
/// This ensures registry auth (`X-Registry-Auth`, `X-Registry-Config`),
/// content negotiation (`Accept`), and other semantically significant
/// headers are preserved when proxying to the guest dockerd.
///
/// Per RFC 7230 §6.1, hop-by-hop headers include the fixed set plus any
/// header names listed in the `Connection` header's value.
fn forward_client_headers(from: &HeaderMap, to: &mut HeaderMap) {
    // Collect header names nominated by the Connection header (RFC 7230 §6.1).
    let mut connection_tokens: Vec<String> = Vec::new();
    if let Some(conn) = from.get(header::CONNECTION) {
        if let Ok(s) = conn.to_str() {
            for token in s.split(',') {
                connection_tokens.push(token.trim().to_ascii_lowercase());
            }
        }
    }

    for (name, value) in from {
        // Skip fixed hop-by-hop headers and Host (overridden later).
        if name == header::CONNECTION
            || name.as_str() == "keep-alive"
            || name.as_str() == "proxy-connection"
            || name == header::PROXY_AUTHENTICATE
            || name == header::PROXY_AUTHORIZATION
            || name == header::TE
            || name == header::TRAILER
            || name == header::TRANSFER_ENCODING
            || name == header::UPGRADE
            || name == header::HOST
        {
            continue;
        }

        // Skip headers nominated by the Connection header.
        if connection_tokens.contains(&name.as_str().to_ascii_lowercase()) {
            continue;
        }

        // Use append to preserve multi-valued headers (e.g. multiple Accept).
        to.append(name.clone(), value.clone());
    }
}

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

    // Forward all non-hop-by-hop headers so registry auth, content negotiation,
    // and other semantics are preserved when proxying to guest dockerd.
    forward_client_headers(headers, req.headers_mut());
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
    let client_headers = req.headers().clone();
    let body = req.into_body();

    let mut guest_req = hyper::Request::builder()
        .method(method)
        .uri(path_and_query)
        .body(body)
        .map_err(|e| DockerError::Server(format!("failed to build guest request: {e}")))?;

    // Forward all non-hop-by-hop headers (registry auth, content negotiation, etc.).
    forward_client_headers(&client_headers, guest_req.headers_mut());
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
