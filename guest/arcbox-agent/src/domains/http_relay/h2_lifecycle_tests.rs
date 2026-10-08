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
