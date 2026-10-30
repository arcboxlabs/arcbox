use super::*;
use std::future::{Future, poll_fn};

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domains::http_tasks;

type RawCall = (Request<h2::RecvStream>, h2::server::SendResponse<Bytes>);

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .unwrap()
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

struct RawFixture {
    sender: h2::client::SendRequest<Bytes>,
    calls: mpsc::UnboundedReceiver<RawCall>,
    cancel: CancellationToken,
    relay: JoinHandle<Result<()>>,
    client: JoinHandle<std::result::Result<(), h2::Error>>,
    backend: JoinHandle<std::result::Result<(), h2::Error>>,
}

impl RawFixture {
    async fn new() -> Self {
        Self::with_window(1).await
    }

    async fn with_window(window: u32) -> Self {
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
        let backend = tokio::spawn(async move {
            let mut connection = h2::server::Builder::new()
                .initial_window_size(window)
                .max_concurrent_streams(1)
                .handshake(upstream)
                .await?;
            while let Some(request) = connection.accept().await {
                calls.send(request?).unwrap();
            }
            Ok(())
        });
        let (sender, connection) = h2::client::handshake(client).await.unwrap();
        Self {
            sender,
            calls: requests,
            cancel,
            relay,
            client: tokio::spawn(connection),
            backend,
        }
    }

    async fn send(
        &self,
        path: &str,
        end: bool,
    ) -> (h2::client::ResponseFuture, h2::SendStream<Bytes>) {
        let mut sender = within(self.sender.clone().ready()).await.unwrap();
        sender
            .send_request(
                Request::post(format!("https://web.arcbox.local/{path}"))
                    .body(())
                    .unwrap(),
                end,
            )
            .unwrap()
    }

    async fn call(&mut self) -> RawCall {
        within(self.calls.recv()).await.unwrap()
    }

    async fn complete_bodyless_request(&mut self, path: &str) {
        let (pending, _) = self.send(path, true).await;
        let (request, mut reply) = self.call().await;
        assert_eq!(request.uri().path(), format!("/{path}"));
        assert!(request.body().is_end_stream());
        reply.send_response(Response::new(()), true).unwrap();
        let mut response = within(pending).await.unwrap().into_body();
        assert!(response.is_end_stream());
        assert!(within(response.data()).await.is_none());
    }

    async fn early_response(
        &mut self,
    ) -> (h2::SendStream<Bytes>, h2::RecvStream, h2::SendStream<Bytes>) {
        // Complete one exchange so the relay has applied the backend's SETTINGS.
        self.complete_bodyless_request("ready").await;
        let (pending, mut upload) = self.send("blocked", false).await;
        upload
            .send_data(Bytes::from_static(&[b'x'; 50]), false)
            .unwrap();
        let (request, mut reply) = self.call().await;
        let mut request = request.into_body();
        assert_eq!(within(request.data()).await.unwrap().unwrap(), &b"x"[..]);
        // Retain the byte's credit: the remaining 49 bytes cannot reach this stream.
        let response_stream = reply.send_response(Response::new(()), true).unwrap();
        let mut response = within(pending).await.unwrap().into_body();
        assert!(response.is_end_stream());
        assert!(within(response.data()).await.is_none());
        assert!(within(response.trailers()).await.unwrap().is_none());
        (upload, request, response_stream)
    }

    async fn shutdown(self) {
        self.cancel.cancel();
        within(self.relay).await.unwrap().unwrap();
        drop(self.sender);
        let _client_close = within(self.client).await.unwrap();
        let _backend_close = within(self.backend).await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_after_response_eos_resets_the_still_blocked_upload() {
    let mut fixture = RawFixture::new().await;
    let (mut upload, mut request, mut response_stream) = fixture.early_response().await;
    upload.send_reset(h2::Reason::CANCEL);
    assert_eq!(
        within(poll_fn(|cx| response_stream.poll_reset(cx)))
            .await
            .unwrap(),
        h2::Reason::CANCEL
    );
    assert_eq!(
        within(request.data()).await.unwrap().unwrap_err().reason(),
        Some(h2::Reason::CANCEL)
    );
    fixture.complete_bodyless_request("sibling").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn response_eos_keeps_the_request_open_for_more_upload_data() {
    let mut fixture = RawFixture::new().await;
    let (mut upload, mut request, _response_stream) = fixture.early_response().await;
    upload
        .send_data(Bytes::from_static(b"after"), true)
        .unwrap();
    request.flow_control().release_capacity(1).unwrap();
    let mut received = vec![b'x'];
    while let Some(data) = within(request.data()).await {
        let data = data.unwrap();
        received.extend_from_slice(&data);
        request.flow_control().release_capacity(data.len()).unwrap();
    }
    let mut expected = vec![b'x'; 50];
    expected.extend_from_slice(b"after");
    assert_eq!(received, expected);
    assert!(within(request.trailers()).await.unwrap().is_none());
    fixture.complete_bodyless_request("sibling").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn cancellation_after_response_eos_resets_an_upload_at_zero_capacity() {
    let mut fixture = RawFixture::with_window(0).await;
    fixture.complete_bodyless_request("ready").await;
    let (pending, mut upload) = fixture.send("zero-window", false).await;
    upload
        .send_data(Bytes::from_static(&[b'x'; 50]), false)
        .unwrap();
    let (request, mut reply) = fixture.call().await;
    let mut request = request.into_body();
    let mut response_stream = reply.send_response(Response::new(()), true).unwrap();
    let mut response = within(pending).await.unwrap().into_body();
    assert!(response.is_end_stream());
    assert!(within(response.data()).await.is_none());
    assert!(within(response.trailers()).await.unwrap().is_none());
    upload.send_reset(h2::Reason::CANCEL);
    assert_eq!(
        within(poll_fn(|cx| response_stream.poll_reset(cx)))
            .await
            .unwrap(),
        h2::Reason::CANCEL
    );
    assert_eq!(
        within(request.data()).await.unwrap().unwrap_err().reason(),
        Some(h2::Reason::CANCEL)
    );
    fixture.complete_bodyless_request("sibling").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn an_http2_early_response_preserves_data_before_the_no_error_reset() {
    let mut fixture = RawFixture::new().await;
    fixture.complete_bodyless_request("ready").await;
    let (pending, mut upload) = fixture.send("early", false).await;
    upload.send_data(Bytes::from_static(b"x"), false).unwrap();
    let (request, mut reply) = fixture.call().await;
    let mut request = request.into_body();
    assert_eq!(within(request.data()).await.unwrap().unwrap(), &b"x"[..]);
    let mut stream = reply
        .send_response(Response::builder().status(413).body(()).unwrap(), false)
        .unwrap();
    stream
        .send_data(Bytes::from_static(b"too big"), true)
        .unwrap();
    drop(request);
    drop(stream);
    drop(reply);
    let response = within(pending).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let mut body = response.into_body();
    let mut received = Vec::new();
    while let Some(data) = within(body.data()).await {
        let data = data.unwrap();
        received.extend_from_slice(&data);
        body.flow_control().release_capacity(data.len()).unwrap();
    }
    assert_eq!(received, b"too big");
    assert!(within(body.trailers()).await.unwrap().is_none());
    assert_eq!(
        within(poll_fn(|cx| upload.poll_reset(cx))).await.unwrap(),
        h2::Reason::NO_ERROR
    );
    fixture.complete_bodyless_request("sibling").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn an_http1_early_close_preserves_the_complete_response_and_stops_upload() {
    use super::tests::{chunk, head, send};
    use tokio::io::AsyncWriteExt as _;

    let (client, incoming) = sockets().await;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let outgoing = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    let relay = tokio::spawn(async move {
        http_tasks::run(&stopping, async move |tasks| {
            serve_managed(
                incoming,
                outgoing,
                Session {
                    client: HttpProtocol::Http2,
                    upstream: HttpProtocol::Http1,
                    tasks,
                    verify: Arc::new(|| Ok(())),
                    invalidate: Arc::new(|| panic!("healthy fixture route was invalidated")),
                },
            )
            .await
        })
        .await
    });
    let backend = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert!(
            head(&mut first)
                .await
                .starts_with("POST /large HTTP/1.1\r\n")
        );
        assert_eq!(chunk(&mut first).await, b"abc");
        send(&mut first, b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 7\r\nConnection: close\r\n\r\ntoo big").await;
        first.shutdown().await.unwrap();
        drop(first);
        let (mut sibling, _) = listener.accept().await.unwrap();
        assert!(
            head(&mut sibling)
                .await
                .starts_with("GET /quick HTTP/1.1\r\n")
        );
        send(
            &mut sibling,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        )
        .await;
        sibling.shutdown().await.unwrap();
    });
    let (mut sender, driver) = h2::client::handshake(client).await.unwrap();
    let client = tokio::spawn(driver);
    let (pending, mut upload) = sender
        .send_request(
            Request::post("https://web.arcbox.local/large")
                .body(())
                .unwrap(),
            false,
        )
        .unwrap();
    upload.send_data(Bytes::from_static(b"abc"), false).unwrap();
    let response = within(pending).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let mut body = response.into_body();
    let mut received = Vec::new();
    while let Some(data) = within(body.data()).await {
        let data = data.unwrap();
        received.extend_from_slice(&data);
        body.flow_control().release_capacity(data.len()).unwrap();
    }
    assert_eq!(received, b"too big");
    assert!(within(body.trailers()).await.unwrap().is_none());
    assert_eq!(
        within(poll_fn(|cx| upload.poll_reset(cx))).await.unwrap(),
        h2::Reason::NO_ERROR
    );
    let mut sender = within(sender.ready()).await.unwrap();
    let (pending, _) = sender
        .send_request(
            Request::get("https://web.arcbox.local/quick")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let mut body = within(pending).await.unwrap().into_body();
    assert_eq!(within(body.data()).await.unwrap().unwrap(), &b"ok"[..]);
    assert!(within(body.data()).await.is_none());
    within(backend).await.unwrap();
    stop.cancel();
    within(relay).await.unwrap().unwrap();
    drop(sender);
    let _client_close = within(client).await.unwrap();
}
