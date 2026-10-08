use super::*;
use std::convert::Infallible;
use std::future::Future;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::Frame;
use hyper::client::conn::http2;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::domains::http_tasks::{self, Executor};

type Body = UnsyncBoxBody<Bytes, Infallible>;
type Frames = mpsc::Sender<std::result::Result<Frame<Bytes>, Infallible>>;
type Call = (Request<Incoming>, oneshot::Sender<Response<Body>>);

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .unwrap()
}

fn channel() -> (Frames, Body) {
    let (sender, receiver) = mpsc::channel(1);
    (
        sender,
        StreamBody::new(ReceiverStream::new(receiver)).boxed_unsync(),
    )
}

fn empty() -> Body {
    Empty::<Bytes>::new().boxed_unsync()
}

async fn frame(sender: &Frames, frame: Frame<Bytes>) {
    within(sender.send(Ok(frame))).await.unwrap();
}

fn trailers(status: &'static str) -> HeaderMap {
    HeaderMap::from_iter([
        (
            HeaderName::from_static("grpc-status"),
            HeaderValue::from_static(status),
        ),
        (
            HeaderName::from_static("grpc-test-bin"),
            HeaderValue::from_static("AP8B"),
        ),
    ])
}

async fn message(body: &mut Incoming, expected: &[u8]) {
    let mut bytes = Vec::new();
    while bytes.len() < expected.len() {
        bytes.extend_from_slice(
            &within(body.frame())
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap(),
        );
    }
    assert_eq!(bytes, expected);
}

async fn end(body: &mut Incoming) {
    let tail = within(body.frame())
        .await
        .unwrap()
        .unwrap()
        .into_trailers()
        .unwrap();
    assert_eq!(tail, trailers("0"));
    assert!(within(body.frame()).await.is_none());
}

async fn sockets() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let (client, server) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (client.unwrap(), server.unwrap().0)
}

struct Fixture {
    sender: http2::SendRequest<Body>,
    calls: mpsc::UnboundedReceiver<Call>,
    cancel: CancellationToken,
    relay: JoinHandle<Result<()>>,
    client: JoinHandle<hyper::Result<()>>,
    backend: JoinHandle<hyper::Result<()>>,
    client_tasks: Executor,
    backend_tasks: Executor,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_window(None).await
    }

    async fn with_window(window: Option<u32>) -> Self {
        let (client, incoming) = sockets().await;
        let (outgoing, upstream) = sockets().await;
        let cancel = CancellationToken::new();
        let stopping = cancel.clone();
        let relay = tokio::spawn(async move {
            http_tasks::run(&stopping, async move |tasks| {
                serve_managed(
                    incoming,
                    outgoing,
                    Session {
                        client: HttpProtocol::Http2,
                        upstream: HttpProtocol::Http2,
                        tasks,
                        verify: Arc::new(|| Ok(())),
                        invalidate: Arc::new(|| panic!("healthy fixture route was invalidated")),
                    },
                )
                .await
            })
            .await
        });
        let (calls, requests) = mpsc::unbounded_channel();
        let backend_tasks = Executor::default();
        let executor = backend_tasks.clone();
        let backend = tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let (reply, response) = oneshot::channel();
                calls.send((request, reply)).unwrap();
                async move { Ok::<_, Infallible>(response.await.unwrap()) }
            });
            let mut builder = hyper::server::conn::http2::Builder::new(executor);
            if let Some(window) = window {
                builder
                    .initial_stream_window_size(window)
                    .max_concurrent_streams(1);
            }
            builder
                .serve_connection(TokioIo::new(upstream), service)
                .await
        });
        let client_tasks = Executor::default();
        let (sender, connection) = http2::handshake(client_tasks.clone(), TokioIo::new(client))
            .await
            .unwrap();
        Self {
            sender,
            calls: requests,
            cancel,
            relay,
            client: tokio::spawn(connection),
            backend,
            client_tasks,
            backend_tasks,
        }
    }

    fn send(&self, method: &str, body: Body) -> JoinHandle<hyper::Result<Response<Incoming>>> {
        let request = Request::post(format!(
            "https://web.arcbox.local/demo.Echo/{method}?wire=1"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("grpc-encoding", "gzip")
        .header("grpc-test-bin", "AP8B")
        .header("x-forwarded-proto", "http")
        .body(body)
        .unwrap();
        let mut sender = self.sender.clone();
        tokio::spawn(async move { sender.send_request(request).await })
    }

    async fn call(&mut self) -> Call {
        within(self.calls.recv()).await.unwrap()
    }

    async fn shutdown(self) {
        self.cancel.cancel();
        within(self.relay).await.unwrap().unwrap();
        // A peer-close error is valid; both drivers must finish before fixture cleanup.
        let _client_close = within(self.client).await.unwrap();
        let _backend_close = within(self.backend).await.unwrap();
        self.client_tasks.shutdown().await;
        self.backend_tasks.shutdown().await;
    }
}

fn response(body: Body) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/grpc")
        .header("grpc-encoding", "gzip")
        .header("grpc-test-bin", "AP8B")
        .body(body)
        .unwrap()
}

const MESSAGE: &[u8] = b"\0\0\0\0\x05hello";

#[tokio::test]
async fn unary_preserves_compressed_messages_binary_metadata_and_trailers() {
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(b"hello").unwrap();
    let gzip = gzip.finish().unwrap();
    let mut compressed = vec![1];
    compressed.extend_from_slice(&u32::try_from(gzip.len()).unwrap().to_be_bytes());
    compressed.extend_from_slice(&gzip);
    let compressed = Bytes::from(compressed);
    let mut fixture = Fixture::new().await;
    let (upload, body) = channel();
    let pending = fixture.send("Unary", body);
    let (request, reply) = fixture.call().await;
    assert_eq!(request.version(), hyper::Version::HTTP_2);
    assert_eq!(request.uri().authority().unwrap(), "web.arcbox.local");
    assert_eq!(
        request.uri().path_and_query().unwrap(),
        "/demo.Echo/Unary?wire=1"
    );
    assert_eq!(request.headers()["grpc-encoding"], "gzip");
    assert_eq!(request.headers()["grpc-test-bin"], "AP8B");
    assert_eq!(request.headers()["te"], "trailers");
    assert_eq!(request.headers()["x-forwarded-proto"], "https");
    assert!(!request.headers().contains_key("connection"));
    frame(&upload, Frame::data(compressed.clone())).await;
    frame(&upload, Frame::trailers(trailers("0"))).await;
    drop(upload);
    let collected = within(request.into_body().collect()).await.unwrap();
    assert_eq!(collected.trailers(), Some(&trailers("0")));
    assert_eq!(collected.to_bytes(), compressed);
    let (download, body) = channel();
    reply.send(response(body)).unwrap();
    let response = within(pending).await.unwrap().unwrap();
    assert_eq!(response.headers()["grpc-encoding"], "gzip");
    assert_eq!(response.headers()["grpc-test-bin"], "AP8B");
    frame(&download, Frame::data(compressed.clone())).await;
    frame(&download, Frame::trailers(trailers("0"))).await;
    let collected = within(response.into_body().collect()).await.unwrap();
    assert_eq!(collected.trailers(), Some(&trailers("0")));
    assert_eq!(collected.to_bytes(), compressed);
    fixture.shutdown().await;
}

#[tokio::test]
async fn client_and_server_streaming_forward_each_message_before_end_of_stream() {
    for (method, requests, responses) in [("ClientStream", 2, 1), ("ServerStream", 1, 2)] {
        let mut fixture = Fixture::new().await;
        let (upload, body) = channel();
        let pending = fixture.send(method, body);
        let (request, reply) = fixture.call().await;
        let mut request = request.into_body();
        for _ in 0..requests {
            frame(&upload, Frame::data(Bytes::from_static(MESSAGE))).await;
            message(&mut request, MESSAGE).await;
        }
        frame(&upload, Frame::trailers(trailers("0"))).await;
        end(&mut request).await;
        let (download, body) = channel();
        reply.send(response(body)).unwrap();
        let mut response = within(pending).await.unwrap().unwrap().into_body();
        for _ in 0..responses {
            frame(&download, Frame::data(Bytes::from_static(MESSAGE))).await;
            message(&mut response, MESSAGE).await;
        }
        frame(&download, Frame::trailers(trailers("0"))).await;
        end(&mut response).await;
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn bidirectional_streaming_keeps_both_message_directions_open() {
    let mut fixture = Fixture::new().await;
    let (upload, body) = channel();
    let pending = fixture.send("Bidi", body);
    let (request, reply) = fixture.call().await;
    let mut request = request.into_body();
    let (download, body) = channel();
    reply.send(response(body)).unwrap();
    let mut response = within(pending).await.unwrap().unwrap().into_body();
    for _ in 0..2 {
        frame(&upload, Frame::data(Bytes::from_static(MESSAGE))).await;
        message(&mut request, MESSAGE).await;
        frame(&download, Frame::data(Bytes::from_static(MESSAGE))).await;
        message(&mut response, MESSAGE).await;
    }
    frame(&upload, Frame::trailers(trailers("0"))).await;
    end(&mut request).await;
    frame(&download, Frame::trailers(trailers("0"))).await;
    end(&mut response).await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn trailers_only_errors_keep_grpc_status_and_message() {
    use hyper::body::Body as _;
    let mut fixture = Fixture::new().await;
    let (upload, body) = channel();
    let pending = fixture.send("Unavailable", body);
    let (request, reply) = fixture.call().await;
    let mut error = response(empty());
    error
        .headers_mut()
        .insert("grpc-status", HeaderValue::from_static("14"));
    error
        .headers_mut()
        .insert("grpc-message", HeaderValue::from_static("try%20later"));
    reply.send(error).unwrap();
    drop(request);
    let mut response = within(pending).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["grpc-status"], "14");
    assert_eq!(response.headers()["grpc-message"], "try%20later");
    assert!(
        response.body().is_end_stream(),
        "trailers-only status must carry END_STREAM on its header block"
    );
    assert!(within(response.body_mut().frame()).await.is_none());
    within(upload.closed()).await;
    fixture.shutdown().await;
}
