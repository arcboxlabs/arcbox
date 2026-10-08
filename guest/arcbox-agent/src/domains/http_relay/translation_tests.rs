use super::tests::{bytes, finish, head, send, sockets, within};
use super::*;
use crate::domains::http_tasks::{self, Executor};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::Version;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn http1_to_http2_preserves_authority_scheme_redirects_and_cookies() {
    let (mut client, incoming) = sockets().await;
    let (outgoing, upstream) = sockets().await;
    let server_tasks = Executor::default();
    let tasks = server_tasks.clone();
    let server = tokio::spawn(async move {
        let service = hyper::service::service_fn(|request: Request<Incoming>| async move {
            assert_eq!(request.version(), Version::HTTP_2);
            assert_eq!(
                request.uri(),
                "https://web.arcbox.local:443/redirect?next=%2Fhome"
            );
            assert_eq!(request.headers()["x-forwarded-proto"], "https");
            assert!(!request.headers().contains_key("x-hop"));
            assert_eq!(request.headers()[header::TE], "trailers");
            assert_eq!(
                request.into_body().collect().await.unwrap().to_bytes(),
                "input"
            );
            Ok::<_, anyhow::Error>(
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "https://web.arcbox.local/next")
                    .header(header::SET_COOKIE, "a=1; Secure")
                    .header(header::SET_COOKIE, "b=2; HttpOnly")
                    .body(Full::new(Bytes::from_static(b"ok")))
                    .unwrap(),
            )
        });
        hyper::server::conn::http2::Builder::new(tasks)
            .serve_connection(TokioIo::new(upstream), service)
            .await
    });
    let relay = tokio::spawn(async move {
        http_tasks::run(&CancellationToken::new(), async |tasks| {
            serve_managed(
                incoming,
                outgoing,
                Session {
                    client: HttpProtocol::Http1,
                    upstream: HttpProtocol::Http2,
                    tasks,
                    verify: Arc::new(|| Ok(())),
                    invalidate: Arc::new(|| panic!("unexpected invalidation")),
                },
            )
            .await
        })
        .await
    });
    send(&mut client, b"POST /redirect?next=%2Fhome HTTP/1.1\r\nHost: web.arcbox.local:443\r\nContent-Length: 5\r\nX-Forwarded-Proto: attacker\r\nConnection: x-hop, TE\r\nX-Hop: secret\r\nTE: trailers\r\n\r\ninput").await;
    let response = head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 302"));
    assert!(response.contains("location: https://web.arcbox.local/next\r\n"));
    assert_eq!(response.matches("set-cookie:").count(), 2);
    assert_eq!(super::tests::chunk(&mut client).await, b"ok");
    assert_eq!(super::tests::chunk(&mut client).await, b"");
    drop(client);
    finish(relay).await;
    let _peer_close = within(server).await.unwrap();
    server_tasks.shutdown().await;
}

type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, std::convert::Infallible>;
type Client = hyper::client::conn::http2::SendRequest<Body>;

struct Http1Backend {
    client: Client,
    listener: tokio::net::TcpListener,
    relay: tokio::task::JoinHandle<Result<()>>,
    stop: CancellationToken,
    client_tasks: Executor,
}

impl Http1Backend {
    async fn new(verify: Arc<dyn Fn() -> Result<()> + Send + Sync>) -> Self {
        use hyper::rt::Executor as _;
        let (client, incoming) = sockets().await;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let outgoing = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let relay = tokio::spawn(async move {
            http_tasks::run(&cancelled, async |tasks| {
                serve_managed(
                    incoming,
                    outgoing,
                    Session {
                        client: HttpProtocol::Http2,
                        upstream: HttpProtocol::Http1,
                        tasks,
                        verify,
                        invalidate: Arc::new(|| panic!("unexpected invalidation")),
                    },
                )
                .await
            })
            .await
        });
        let client_tasks = Executor::default();
        let (client, driver) =
            hyper::client::conn::http2::handshake(client_tasks.clone(), TokioIo::new(client))
                .await
                .unwrap();
        client_tasks.execute(async {
            let _peer_close = driver.await;
        });
        Self {
            client,
            listener,
            relay,
            stop,
            client_tasks,
        }
    }
}

fn request(path: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("https://web.arcbox.local:443{path}"))
        .header(header::TE, "trailers")
        .body(http_body_util::Empty::new().boxed_unsync())
        .unwrap()
}

#[tokio::test]
async fn http2_to_http1_keeps_sse_and_sibling_streams_independent() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut fixture = Http1Backend::new(Arc::new(|| Ok(()))).await;
    let (first_closed, closed) = tokio::sync::oneshot::channel();
    let listener = fixture.listener;
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let received = head(&mut first).await;
        assert!(received.starts_with("GET /events HTTP/1.1\r\n"));
        assert!(received.contains("host: web.arcbox.local:443\r\n"));
        assert!(received.contains("te: trailers\r\n"));
        send(&mut first, b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nB\r\ndata: one\n\n\r\n").await;
        let first = tokio::spawn(async move {
            first.read_to_end(&mut Vec::new()).await.unwrap();
            first_closed.send(()).unwrap();
        });
        let (mut second, _) = listener.accept().await.unwrap();
        assert!(
            head(&mut second)
                .await
                .starts_with("GET /quick HTTP/1.1\r\n")
        );
        send(
            &mut second,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        )
        .await;
        second.shutdown().await.unwrap();
        within(first).await.unwrap();
    });
    let mut first = within(fixture.client.send_request(request("/events")))
        .await
        .unwrap();
    assert_eq!(
        within(first.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        b"data: one\n\n"[..]
    );
    let second = within(fixture.client.send_request(request("/quick")))
        .await
        .unwrap();
    assert_eq!(
        within(second.into_body().collect())
            .await
            .unwrap()
            .to_bytes(),
        b"ok"[..]
    );
    drop(first);
    within(closed).await.unwrap();
    within(server).await.unwrap();
    fixture.stop.cancel();
    finish(fixture.relay).await;
    fixture.client_tasks.shutdown().await;
}

#[tokio::test]
async fn an_extra_http1_connection_rechecks_ownership_before_forwarding() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt as _;
    let checks = Arc::new(AtomicUsize::new(0));
    let verify = {
        let checks = checks.clone();
        Arc::new(move || {
            ensure!(
                checks.fetch_add(1, Ordering::SeqCst) != 3,
                "route was replaced during connect"
            );
            Ok(())
        })
    };
    let mut fixture = Http1Backend::new(verify).await;
    let listener = fixture.listener;
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        head(&mut first).await;
        send(&mut first, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        let (mut replaced, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        within(replaced.read_to_end(&mut bytes)).await.unwrap();
        assert!(
            bytes.is_empty(),
            "a replaced route received application bytes"
        );
    });
    within(fixture.client.send_request(request("/first")))
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap();
    assert!(
        within(fixture.client.send_request(request("/replaced")))
            .await
            .is_err()
    );
    within(server).await.unwrap();
    assert_eq!(checks.load(Ordering::SeqCst), 4);
    fixture.stop.cancel();
    finish(fixture.relay).await;
    fixture.client_tasks.shutdown().await;
}

#[tokio::test]
async fn a_dual_protocol_backend_keeps_http1_websocket_upgrade_frames() {
    use hyper::rt::Executor as _;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::{TcpListener, TcpStream};
    let (mut client, incoming) = sockets().await;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let outgoing = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let backend_tasks = Executor::default();
    let tasks = backend_tasks.clone();
    let server = tokio::spawn(async move {
        let (h2, _) = listener.accept().await.unwrap();
        let executor = tasks.clone();
        tasks.execute(async move {
            let service = hyper::service::service_fn(|_: Request<Incoming>| async {
                Err::<Response<Full<Bytes>>, _>(anyhow::anyhow!(
                    "WebSocket request must use its own HTTP/1 connection"
                ))
            });
            let _peer_close = hyper::server::conn::http2::Builder::new(executor)
                .serve_connection(TokioIo::new(h2), service)
                .await;
        });
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = head(&mut socket).await;
        assert!(request.contains("upgrade: websocket\r\n"));
        send(&mut socket, b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n\x81\x02hi").await;
        assert_eq!(bytes(&mut socket, 8).await, b"\x81\x82\x01\x02\x03\x04ik");
        socket.read_to_end(&mut Vec::new()).await.unwrap();
    });
    let relay = tokio::spawn(async move {
        http_tasks::run(&CancellationToken::new(), async |tasks| {
            serve_managed(
                incoming,
                outgoing,
                Session {
                    client: HttpProtocol::Http1,
                    upstream: HttpProtocol::Http2,
                    tasks,
                    verify: Arc::new(|| Ok(())),
                    invalidate: Arc::new(|| panic!("unexpected invalidation")),
                },
            )
            .await
        })
        .await
    });
    send(&mut client, b"GET /socket HTTP/1.1\r\nHost: web.arcbox.local\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n\x81\x82\x01\x02\x03\x04ik").await;
    assert!(head(&mut client).await.starts_with("HTTP/1.1 101"));
    assert_eq!(bytes(&mut client, 4).await, b"\x81\x02hi");
    drop(client);
    finish(relay).await;
    within(server).await.unwrap();
    backend_tasks.shutdown().await;
}
