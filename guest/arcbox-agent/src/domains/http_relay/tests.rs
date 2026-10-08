use super::*;
use std::future::Future;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(2), future)
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

async fn fixture() -> (TcpStream, TcpStream, JoinHandle<Result<()>>) {
    let (client, incoming) = sockets().await;
    let (outgoing, upstream) = sockets().await;
    (client, upstream, tokio::spawn(serve(incoming, outgoing)))
}

async fn until(stream: &mut TcpStream, end: &[u8]) -> Vec<u8> {
    within(async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(end) {
            bytes.push(stream.read_u8().await.unwrap());
        }
        bytes
    })
    .await
}

async fn head(stream: &mut TcpStream) -> String {
    String::from_utf8(until(stream, b"\r\n\r\n").await).unwrap()
}

async fn send(stream: &mut TcpStream, bytes: &[u8]) {
    within(stream.write_all(bytes)).await.unwrap();
}

async fn bytes(stream: &mut TcpStream, count: usize) -> Vec<u8> {
    let mut bytes = vec![0; count];
    within(stream.read_exact(&mut bytes)).await.unwrap();
    bytes
}

async fn chunk(stream: &mut TcpStream) -> Vec<u8> {
    let line = String::from_utf8(until(stream, b"\r\n").await).unwrap();
    let count = usize::from_str_radix(line.trim(), 16).unwrap();
    let data = bytes(stream, count).await;
    assert_eq!(bytes(stream, 2).await, b"\r\n");
    data
}

async fn finish(task: JoinHandle<Result<()>>) {
    within(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn keepalive_overwrites_scheme_and_preserves_host_redirects_and_cookies() {
    let (mut client, mut upstream, task) = fixture().await;
    for _ in 0..2 {
        send(&mut client, b"GET /redirect?next=%2Fhome HTTP/1.1\r\nHost: web.arcbox.local:443\r\nX-Forwarded-Proto: http\r\nX-Forwarded-Proto: attacker\r\nConnection: keep-alive, X-Forwarded-Proto, x-hop, Host\r\nX-Hop: secret\r\n\r\n").await;
    }
    for last in [false, true] {
        let request = head(&mut upstream).await.to_ascii_lowercase();
        assert!(request.starts_with("get /redirect?next=%2fhome http/1.1\r\n"));
        assert!(request.contains("host: web.arcbox.local:443\r\n"));
        assert_eq!(request.matches("x-forwarded-proto:").count(), 1);
        assert!(request.contains("x-forwarded-proto: https\r\n"));
        assert!(!request.contains("x-hop:"));
        send(&mut upstream, b"HTTP/1.1 302 Found\r\nContent-Length: 2\r\nLocation: http://web.arcbox.local/path?x=1\r\nSet-Cookie: a=1; Secure; Path=/\r\nSet-Cookie: b=2; HttpOnly\r\nConnection: x-private\r\nX-Private: secret\r\n\r\nok").await;
        if last {
            upstream.shutdown().await.unwrap();
        }
        let response = head(&mut client).await;
        assert!(response.contains("location: http://web.arcbox.local/path?x=1\r\n"));
        assert_eq!(response.matches("set-cookie:").count(), 2);
        assert!(response.contains("set-cookie: a=1; Secure; Path=/\r\n"));
        assert!(response.contains("set-cookie: b=2; HttpOnly\r\n"));
        assert!(!response.contains("x-private:"));
        assert_eq!(bytes(&mut client, 2).await, b"ok");
    }
    finish(task).await;
    assert_eq!(within(client.read(&mut [0])).await.unwrap(), 0);
}

#[tokio::test]
async fn upload_and_sse_frames_stream_before_the_bodies_finish() {
    let (mut client, mut upstream, task) = fixture().await;
    send(&mut client, b"POST /events HTTP/1.1\r\nHost: web.arcbox.local\r\nContent-Length: 6\r\nTE: trailers\r\nConnection: TE\r\n\r\nabc").await;
    let request = head(&mut upstream).await;
    assert!(request.contains("te: trailers\r\n"));
    assert!(request.contains("connection: te\r\n"));
    assert_eq!(bytes(&mut upstream, 3).await, b"abc");
    send(&mut client, b"def").await;
    assert_eq!(bytes(&mut upstream, 3).await, b"def");
    send(&mut upstream, b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nTrailer: x-end\r\n\r\nB\r\ndata: one\n\n\r\n").await;
    let response = head(&mut client).await;
    assert!(response.contains("content-type: text/event-stream\r\n"));
    assert_eq!(chunk(&mut client).await, b"data: one\n\n");
    send(&mut upstream, b"B\r\ndata: two\n\n\r\n").await;
    assert_eq!(chunk(&mut client).await, b"data: two\n\n");
    send(&mut upstream, b"0\r\nx-end: done\r\n\r\n").await;
    assert_eq!(
        until(&mut client, b"\r\n\r\n").await,
        b"0\r\nx-end: done\r\n\r\n"
    );
    drop(client);
    finish(task).await;
    assert_eq!(within(upstream.read(&mut [0])).await.unwrap(), 0);
}

#[tokio::test]
async fn websocket_preserves_bidirectional_frames_buffered_with_the_upgrade() {
    let (mut client, mut upstream, task) = fixture().await;
    let client_frame = b"\x81\x82\x01\x02\x03\x04ik";
    let server_frame = b"\x81\x02hi";
    let mut request = b"GET /socket HTTP/1.1\r\nHost: web.arcbox.local\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n".to_vec();
    request.extend_from_slice(client_frame);
    send(&mut client, &request).await;
    let request = head(&mut upstream).await;
    assert!(request.contains("upgrade: websocket\r\n"));
    assert!(request.contains("x-forwarded-proto: https\r\n"));
    let mut response = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n".to_vec();
    response.extend_from_slice(server_frame);
    send(&mut upstream, &response).await;
    assert!(head(&mut client).await.starts_with("HTTP/1.1 101"));
    assert_eq!(bytes(&mut client, server_frame.len()).await, server_frame);
    assert_eq!(bytes(&mut upstream, client_frame.len()).await, client_frame);
    drop(client);
    drop(upstream);
    finish(task).await;
}

#[tokio::test]
async fn an_early_response_keeps_the_next_request_behind_the_upload() {
    let (mut client, mut upstream, task) = fixture().await;
    send(
        &mut client,
        b"POST /early HTTP/1.1\r\nHost: web.arcbox.local\r\nContent-Length: 6\r\n\r\nabc",
    )
    .await;
    head(&mut upstream).await;
    assert_eq!(bytes(&mut upstream, 3).await, b"abc");
    send(
        &mut upstream,
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(head(&mut client).await.starts_with("HTTP/1.1 200"));
    send(
        &mut client,
        b"defGET /next HTTP/1.1\r\nHost: web.arcbox.local\r\n\r\n",
    )
    .await;
    assert_eq!(bytes(&mut upstream, 3).await, b"def");
    let request = head(&mut upstream).await;
    assert!(request.starts_with("GET /next HTTP/1.1\r\n"));
    send(
        &mut upstream,
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnext",
    )
    .await;
    upstream.shutdown().await.unwrap();
    head(&mut client).await;
    assert_eq!(bytes(&mut client, 4).await, b"next");
    finish(task).await;
}

#[tokio::test]
async fn cancelling_or_disconnecting_a_stream_closes_its_upstream_driver() {
    for abort in [false, true] {
        let (mut client, mut upstream, task) = fixture().await;
        send(
            &mut client,
            b"GET /events HTTP/1.1\r\nHost: web.arcbox.local\r\n\r\n",
        )
        .await;
        head(&mut upstream).await;
        send(
            &mut upstream,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
        )
        .await;
        head(&mut client).await;
        assert_eq!(chunk(&mut client).await, b"x");
        if abort {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(within(client.read(&mut [0])).await.unwrap(), 0);
        } else {
            drop(client);
            // A peer-close error is valid; task completion and upstream EOF prove cleanup.
            let _peer_close = within(task).await.unwrap();
        }
        assert_eq!(within(upstream.read(&mut [0])).await.unwrap(), 0);
    }
}
