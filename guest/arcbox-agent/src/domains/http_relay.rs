//! HTTP forwarding after managed TLS termination.

mod exchange;
mod h1_body;
mod h2_transport;
mod multiplex;

pub(super) use multiplex::{Session, serve as serve_managed};

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc};

use super::HttpProtocol;

/// Owns both HTTP drivers and the optional upgraded tunnel until the client leaves.
pub(super) async fn serve(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    upstream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
) -> Result<()> {
    let (sender, driver) = hyper::client::conn::http1::handshake(TokioIo::new(upstream)).await?;
    let sender = Arc::new(Mutex::new(sender));
    let (upgrades, mut upgraded) = mpsc::unbounded_channel();
    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        let sender = Arc::clone(&sender);
        let upgrades = upgrades.clone();
        async move {
            let websocket = prepare_request(&mut request, HttpProtocol::Http1)?;
            let client_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
            let mut sender = sender.lock().await;
            sender.ready().await?;
            let mut response = sender.send_request(request).await?;
            if prepare_response(&mut response, websocket)? {
                upgrades.send((
                    client_upgrade.context("WebSocket client upgrade missing")?,
                    hyper::upgrade::on(&mut response),
                ))?;
            }
            Ok::<_, anyhow::Error>(response)
        }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    let mut server = Box::pin(
        builder
            .serve_connection(TokioIo::new(client), service)
            .with_upgrades(),
    );
    let driver = driver.with_upgrades();
    tokio::pin!(driver);
    tokio::select! {
        result = &mut server => {
            result?;
            // Drop the service's sender before waiting for the upstream driver to close.
            drop(server);
            driver.await?;
        }
        result = &mut driver => {
            result?;
            // A closed upstream can still have a complete response queued for the client.
            server.as_mut().graceful_shutdown();
            server.await?;
        }
    }
    if let Some((client, upstream)) = upgraded.recv().await {
        // The HTTP drivers must flush 101 before either upgrade can finish.
        let (client, upstream) = tokio::try_join!(client, upstream)?;
        tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream))
            .await?;
    }
    Ok(())
}

fn prepare_request<B>(request: &mut Request<B>, protocol: HttpProtocol) -> Result<bool> {
    let trailers = accepts_trailers(request.headers());
    let upgrade = request.headers().get(header::UPGRADE).cloned();
    let websocket =
        strip_hop_headers(request.headers_mut())? && upgrade.as_ref().is_some_and(is_websocket);
    if websocket {
        set_upgrade(request.headers_mut());
    }
    if trailers {
        // TE is negotiated per hop; retain only the client's trailer capability.
        request
            .headers_mut()
            .insert(header::TE, HeaderValue::from_static("trailers"));
        if protocol == HttpProtocol::Http1 {
            request
                .headers_mut()
                .append(header::CONNECTION, HeaderValue::from_static("te"));
        }
    }
    request
        .headers_mut()
        .insert("x-forwarded-proto", HeaderValue::from_static("https"));
    Ok(websocket)
}

fn accepts_trailers(headers: &HeaderMap) -> bool {
    headers.get_all(header::TE).iter().any(|value| {
        value
            .as_bytes()
            .split(|byte| *byte == b',')
            .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"trailers"))
    })
}

fn prepare_response<B>(response: &mut Response<B>, websocket: bool) -> Result<bool> {
    let protocol = response.headers().get(header::UPGRADE).cloned();
    let switching =
        strip_hop_headers(response.headers_mut())? && protocol.as_ref().is_some_and(is_websocket);
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        ensure!(
            websocket && switching,
            "upstream returned an unsolicited upgrade"
        );
        set_upgrade(response.headers_mut());
        return Ok(true);
    }
    Ok(false)
}

fn is_websocket(value: &HeaderValue) -> bool {
    value.as_bytes().eq_ignore_ascii_case(b"websocket")
}

fn set_upgrade(headers: &mut HeaderMap) {
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
}

/// Keeps Host and Trailer because routing and streamed trailer frames still need their declarations.
fn strip_hop_headers(headers: &mut HeaderMap) -> Result<bool> {
    let mut nominated = Vec::new();
    for connection in headers.get_all(header::CONNECTION) {
        for name in connection
            .to_str()?
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            nominated.push(HeaderName::from_bytes(name.as_bytes())?);
        }
    }
    let upgrade = nominated.contains(&header::UPGRADE);
    for name in nominated {
        if name != header::HOST && name != header::TRAILER {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
    ] {
        headers.remove(name);
    }
    Ok(upgrade)
}

#[cfg(test)]
mod tests;
