//! Stream-preserving forwarding when either HTTP peer uses HTTP/2.

use std::future::poll_fn;
use std::net::SocketAddr;

use bytes::Bytes;
use hyper::Version;
use hyper::body::Body as _;
use hyper::client::conn::http1;
use hyper::rt::Executor as _;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::h1_body::Body;
use super::h2_transport::reset_error;
use super::*;
use crate::domains::http_tasks::Executor;

pub(in crate::domains) struct Session {
    pub client: HttpProtocol,
    pub upstream: HttpProtocol,
    pub tasks: Executor,
    pub verify: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    pub invalidate: Arc<dyn Fn() + Send + Sync>,
}

pub(in crate::domains) async fn serve(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    upstream: TcpStream,
    session: Session,
) -> Result<()> {
    if session.client == HttpProtocol::Http1 && session.upstream == HttpProtocol::Http1 {
        return super::serve(client, upstream).await;
    }
    let address = upstream.peer_addr()?;
    let tasks = session.tasks.clone();
    let client_protocol = session.client;
    let (first, sender) = if session.upstream == HttpProtocol::Http2 {
        let (sender, driver) = h2::client::Builder::new()
            .enable_push(false)
            .initial_max_send_streams(100)
            .initial_window_size(2 * 1024 * 1024)
            .initial_connection_window_size(5 * 1024 * 1024)
            .max_header_list_size(16 * 1024)
            .max_send_buffer_size(1024 * 1024)
            .handshake(upstream)
            .await?;
        retain(&tasks, driver);
        (None, Some(sender))
    } else {
        (Some(upstream), None)
    };
    let backend = Arc::new(Backend {
        address,
        first: Mutex::new(first),
        sender,
        session,
    });
    if client_protocol == HttpProtocol::Http2 {
        let mut connection = h2::server::Builder::new()
            .max_concurrent_streams(200)
            .initial_window_size(1024 * 1024)
            .initial_connection_window_size(1024 * 1024)
            .max_header_list_size(16 * 1024)
            .handshake(client)
            .await?;
        while let Some(stream) = connection.accept().await {
            let (request, response) = stream?;
            let backend = Arc::clone(&backend);
            tasks.execute(async move {
                if let Err(error) = forward_h2(backend, request, response).await {
                    tracing::debug!(%error, "managed HTTP/2 stream ended");
                }
            });
        }
        return Ok(());
    }
    let (upgrades, mut upgraded) = mpsc::unbounded_channel();
    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        let backend = Arc::clone(&backend);
        let upgrades = upgrades.clone();
        async move {
            let accepts_trailers = accepts_trailers(request.headers());
            let head = request.method() == hyper::Method::HEAD;
            let protocol = if request
                .headers()
                .get(header::UPGRADE)
                .is_some_and(is_websocket)
            {
                HttpProtocol::Http1
            } else {
                backend.session.upstream
            };
            let websocket = prepare_request(&mut request, protocol)?;
            let client_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
            translate_request(&mut request, protocol)?;
            let cancel = CancellationToken::new();
            let cancellation = cancel.clone().drop_guard();
            let (mut response, _uploaded) = backend
                .send(request.map(h1_body::direct), protocol, cancel)
                .await?;
            if prepare_response(&mut response, websocket)? {
                upgrades.send((
                    client_upgrade.context("WebSocket client upgrade missing")?,
                    hyper::upgrade::on(&mut response),
                ))?;
            }
            *response.version_mut() = Version::HTTP_11;
            let response = if protocol == HttpProtocol::Http2
                && !head
                && !response.status().is_informational()
                && response.status() != StatusCode::NO_CONTENT
                && response.status() != StatusCode::NOT_MODIFIED
            {
                let (mut parts, body) = response.into_parts();
                let body = h1_body::stream(body, &mut parts.headers, accepts_trailers)?;
                Response::from_parts(parts, body)
            } else {
                response
            };
            Ok::<_, anyhow::Error>(response.map(|body| exchange::guard(body, cancellation)))
        }
    });
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .serve_connection(TokioIo::new(client), service)
        .with_upgrades()
        .await?;
    if let Some((client, upstream)) = upgraded.recv().await {
        let (client, upstream) = tokio::try_join!(client, upstream)?;
        tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream))
            .await?;
    }
    Ok(())
}

async fn forward_h2(
    backend: Arc<Backend>,
    request: Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let cancellation = cancel.clone().drop_guard();
    let mut output = None;
    let result = async {
        let protocol = backend.session.upstream;
        let mut request = request.map(h2_transport::body);
        prepare_request(&mut request, protocol)?;
        translate_request(&mut request, protocol)?;
        if protocol == HttpProtocol::Http1 {
            let (mut parts, body) = request.into_parts();
            let body = h1_body::stream(body, &mut parts.headers, true)?;
            request = Request::from_parts(parts, body);
        }
        let (mut response, uploaded) = tokio::select! {
            result = backend.send(request, protocol, cancel) => result?,
            reset = poll_fn(|cx| respond.poll_reset(cx)) => return Err(reset_error(reset)),
        };
        prepare_response(&mut response, false)?;
        let end = response.body().is_end_stream();
        let (parts, body) = response.into_parts();
        output = Some(respond.send_response(Response::from_parts(parts, ()), end)?);
        let stream = output.as_mut().expect("response stream was opened");
        if !end {
            h2_transport::send_body(stream, body).await?;
        }
        // Response EOS does not end an upload or release ownership of its reset signal.
        tokio::select! {
            result = uploaded => match result.context("upstream upload task ended without a result")? {
                Ok(exchange::Upload::Complete | exchange::Upload::Abandoned) => {},
                Err(error) if error.downcast_ref::<h2::Error>().and_then(h2::Error::reason)
                    == Some(h2::Reason::NO_ERROR) => {},
                Err(error) => return Err(error),
            },
            reset = poll_fn(|cx| stream.poll_reset(cx)) => return Err(reset_error(reset)),
        }
        // Drop the final handles so h2 flushes the complete response before an implicit NO_ERROR reset.
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        let reason = error
            .downcast_ref::<h2::Error>()
            .and_then(h2::Error::reason)
            .unwrap_or(h2::Reason::INTERNAL_ERROR);
        if let Some(mut stream) = output {
            stream.send_reset(reason);
        } else {
            respond.send_reset(reason);
        }
    } else {
        cancellation.disarm();
    }
    result
}

struct Backend {
    address: SocketAddr,
    first: Mutex<Option<TcpStream>>,
    sender: Option<h2::client::SendRequest<Bytes>>,
    session: Session,
}

impl Backend {
    async fn send(
        &self,
        request: Request<Body>,
        protocol: HttpProtocol,
        cancel: CancellationToken,
    ) -> Result<(Response<Body>, exchange::Uploaded)> {
        (self.session.verify)()?;
        if protocol == HttpProtocol::Http2 {
            let mut sender = self
                .sender
                .as_ref()
                .context("HTTP/2 sender missing")?
                .clone()
                .ready()
                .await?;
            (self.session.verify)()?;
            let (parts, body) = request.into_parts();
            let end = body.is_end_stream();
            let (response, mut stream) =
                sender.send_request(Request::from_parts(parts, ()), end)?;
            let (completed, uploaded) = oneshot::channel();
            self.session.tasks.execute(async move {
                let result = if end {
                    Ok(())
                } else {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            stream.send_reset(h2::Reason::CANCEL);
                            Err(h2::Error::from(h2::Reason::CANCEL).into())
                        }
                        result = h2_transport::send_body(&mut stream, body) => result,
                    }
                };
                let _receiver_closed = completed.send(result.map(|()| exchange::Upload::Complete));
            });
            return Ok((response.await?.map(h2_transport::body), uploaded));
        }
        let first = self.first.lock().await.take();
        let upstream = if let Some(first) = first {
            first
        } else {
            let result =
                tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(self.address))
                    .await
                    .context("connecting to the container timed out")
                    .and_then(|result| result.context("connecting to the container"));
            (self.session.verify)()?;
            match result {
                Ok(stream) => stream,
                Err(error) => {
                    (self.session.invalidate)();
                    return Err(error);
                }
            }
        };
        let (mut sender, driver) = http1::handshake(TokioIo::new(upstream)).await?;
        retain(&self.session.tasks, async move {
            tokio::select! {
                () = cancel.cancelled() => Ok(()),
                result = driver.with_upgrades() => result,
            }
        });
        (self.session.verify)()?;
        let (parts, body) = request.into_parts();
        let (body, uploaded) = exchange::track(body);
        let response = sender
            .send_request(Request::from_parts(parts, body))
            .await?;
        Ok((response.map(h1_body::direct), uploaded))
    }
}

fn retain<E: std::fmt::Display + Send + 'static>(
    tasks: &Executor,
    driver: impl std::future::Future<Output = std::result::Result<(), E>> + Send + 'static,
) {
    tasks.execute(async move {
        if let Err(error) = driver.await {
            tracing::debug!(%error, "managed HTTP upstream driver ended");
        }
    });
}

fn translate_request<B>(request: &mut Request<B>, protocol: HttpProtocol) -> Result<()> {
    let authority = request
        .uri()
        .authority()
        .cloned()
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)?
                .to_str()
                .ok()?
                .parse()
                .ok()
        })
        .context("HTTP request omitted authority")?;
    let path = request
        .uri()
        .path_and_query()
        .cloned()
        .context("HTTP request omitted path")?;
    *request.uri_mut() = match protocol {
        HttpProtocol::Http1 => {
            request
                .headers_mut()
                .insert(header::HOST, HeaderValue::from_str(authority.as_str())?);
            hyper::Uri::builder().path_and_query(path).build()?
        }
        HttpProtocol::Http2 => {
            request.headers_mut().remove(header::HOST);
            hyper::Uri::builder()
                .scheme("https")
                .authority(authority)
                .path_and_query(path)
                .build()?
        }
    };
    *request.version_mut() = match protocol {
        HttpProtocol::Http1 => Version::HTTP_11,
        HttpProtocol::Http2 => Version::HTTP_2,
    };
    Ok(())
}
