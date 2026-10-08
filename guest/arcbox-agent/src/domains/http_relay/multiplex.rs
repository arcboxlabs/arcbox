//! Stream-preserving forwarding when either HTTP peer uses HTTP/2.

use std::net::SocketAddr;

use hyper::Version;
use hyper::client::conn::{http1, http2};
use hyper::rt::Executor as _;
use tokio::net::TcpStream;

use super::h1_body::Body;
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
        let (sender, driver) = http2::handshake(tasks.clone(), TokioIo::new(upstream)).await?;
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
    let (upgrades, mut upgraded) = mpsc::unbounded_channel();
    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        let backend = backend.clone();
        let upgrades = upgrades.clone();
        async move {
            let accepts_trailers = accepts_trailers(request.headers());
            let head = request.method() == hyper::Method::HEAD;
            let upgrade = client_protocol == HttpProtocol::Http1
                && request
                    .headers()
                    .get(header::UPGRADE)
                    .is_some_and(is_websocket);
            let protocol = if upgrade {
                HttpProtocol::Http1
            } else {
                backend.session.upstream
            };
            let websocket = prepare_request(&mut request, protocol)?;
            let client_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
            translate_request(&mut request, protocol)?;
            let request =
                if client_protocol == HttpProtocol::Http2 && protocol == HttpProtocol::Http1 {
                    let (mut parts, body) = request.into_parts();
                    let body = h1_body::stream(body, &mut parts.headers, true)?;
                    Request::from_parts(parts, body)
                } else {
                    request.map(h1_body::direct)
                };
            let mut response = backend.send(request, protocol).await?;
            if prepare_response(&mut response, websocket)? {
                upgrades.send((
                    client_upgrade.context("WebSocket client upgrade missing")?,
                    hyper::upgrade::on(&mut response),
                ))?;
            }
            *response.version_mut() = version(client_protocol);
            let response = if client_protocol == HttpProtocol::Http1
                && protocol == HttpProtocol::Http2
                && !head
                && !response.status().is_informational()
                && response.status() != StatusCode::NO_CONTENT
                && response.status() != StatusCode::NOT_MODIFIED
            {
                let (mut parts, body) = response.into_parts();
                let body = h1_body::stream(body, &mut parts.headers, accepts_trailers)?;
                Response::from_parts(parts, body)
            } else {
                response.map(h1_body::direct)
            };
            Ok::<_, anyhow::Error>(response)
        }
    });
    match client_protocol {
        HttpProtocol::Http1 => {
            hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .serve_connection(TokioIo::new(client), service)
                .with_upgrades()
                .await?;
        }
        HttpProtocol::Http2 => {
            hyper::server::conn::http2::Builder::new(tasks)
                .serve_connection(TokioIo::new(client), service)
                .await?;
        }
    }
    if client_protocol == HttpProtocol::Http1
        && let Some((client, upstream)) = upgraded.recv().await
    {
        let (client, upstream) = tokio::try_join!(client, upstream)?;
        tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream))
            .await?;
    }
    Ok(())
}

struct Backend {
    address: SocketAddr,
    first: Mutex<Option<TcpStream>>,
    sender: Option<http2::SendRequest<Body>>,
    session: Session,
}

impl Backend {
    async fn send(
        &self,
        request: Request<Body>,
        protocol: HttpProtocol,
    ) -> Result<Response<Incoming>> {
        (self.session.verify)()?;
        if protocol == HttpProtocol::Http2 {
            let mut sender = self
                .sender
                .as_ref()
                .context("HTTP/2 sender missing")?
                .clone();
            sender.ready().await?;
            (self.session.verify)()?;
            return Ok(sender.send_request(request).await?);
        }
        // One HTTP/1 socket per stream lets an SSE response coexist with sibling HTTP/2 requests.
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
        retain(&self.session.tasks, driver.with_upgrades());
        (self.session.verify)()?;
        Ok(sender.send_request(request).await?)
    }
}

fn retain(
    tasks: &Executor,
    driver: impl std::future::Future<Output = hyper::Result<()>> + Send + 'static,
) {
    tasks.execute(async move {
        // Request and body futures carry failures to the affected stream.
        if let Err(error) = driver.await {
            tracing::debug!(%error, "managed HTTP upstream driver ended");
        }
    });
}

fn version(protocol: HttpProtocol) -> Version {
    match protocol {
        HttpProtocol::Http1 => Version::HTTP_11,
        HttpProtocol::Http2 => Version::HTTP_2,
    }
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
    *request.version_mut() = version(protocol);
    Ok(())
}
