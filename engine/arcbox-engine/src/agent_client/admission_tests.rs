use std::io::{Read, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;

use super::*;

fn blocking_pair() -> (AgentClient, UnixStream) {
    let (host, guest) = UnixStream::pair().unwrap();
    // SAFETY: the transport receives ownership of this connected socket.
    let transport = unsafe {
        arcbox_transport::vsock::BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap()
    };
    (
        AgentClient {
            cid: 3,
            transport: AgentTransport::Blocking(transport),
            connected: true,
            protocol_admitted: false,
        },
        guest,
    )
}

fn receive(guest: &mut UnixStream, expected: MessageType) {
    guest
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut length = [0; 4];
    guest.read_exact(&mut length).unwrap();
    let mut frame = vec![0; u32::from_be_bytes(length) as usize];
    guest.read_exact(&mut frame).unwrap();
    assert_eq!(
        u32::from_be_bytes(frame[..4].try_into().unwrap()),
        expected as u32
    );
}

fn respond(guest: &mut UnixStream, kind: MessageType, payload: &[u8]) {
    guest
        .write_all(&wire::build_message(kind, "", payload))
        .unwrap();
}

fn handshake(guest: &mut UnixStream, version: u32) {
    receive(guest, MessageType::PingRequest);
    respond(
        guest,
        MessageType::PingResponse,
        &PingResponse {
            protocol_version: version,
            version: "test-agent".into(),
            message: "negotiation detail".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
}

fn unary_peer(mut guest: UnixStream, version: u32, explicit_ping: bool) {
    handshake(&mut guest, version);
    if version == 6 {
        if explicit_ping {
            handshake(&mut guest, version);
        }
        assert_eq!(
            guest.read(&mut [0; 1]).unwrap(),
            0,
            "v6 received a business frame"
        );
    } else {
        for _ in 0..2 {
            receive(&mut guest, MessageType::GetSystemInfoRequest);
            respond(&mut guest, MessageType::GetSystemInfoResponse, &[]);
        }
    }
}

fn assert_rejected(result: Result<SystemInfo>) {
    let error = result
        .expect_err("v6 must not receive a business request")
        .to_string();
    for detail in ["requires >= 7", "test-agent", "negotiation detail"] {
        assert!(error.contains(detail), "{error}");
    }
}

#[tokio::test]
async fn failed_blocking_handshake_closes_the_connection() {
    for (kind, payload) in [
        (MessageType::GetSystemInfoResponse, vec![]),
        (MessageType::PingResponse, vec![0xff]),
    ] {
        let (mut client, mut guest) = blocking_pair();
        let peer = std::thread::spawn(move || {
            receive(&mut guest, MessageType::PingRequest);
            respond(&mut guest, kind, &payload);
            assert_eq!(guest.read(&mut [0]).unwrap(), 0);
        });
        assert!(client.get_system_info_blocking().is_err());
        assert!(!client.connected);
        assert!(
            client
                .connect()
                .await
                .unwrap_err()
                .to_string()
                .contains("obtain a new VM socket")
        );
        assert!(client.get_system_info_blocking().is_err());
        peer.join().unwrap();
    }
}

#[test]
fn blocking_unary_admits_v7_once_and_rejects_v6_after_explicit_ping() {
    for version in [6, 7] {
        for explicit_ping in [false, true] {
            let (mut client, guest) = blocking_pair();
            let peer = std::thread::spawn(move || unary_peer(guest, version, explicit_ping));
            if explicit_ping {
                assert_eq!(client.ping_blocking().unwrap().protocol_version, version);
            }
            if version == 6 {
                assert_rejected(client.get_system_info_blocking());
            } else {
                client.get_system_info_blocking().unwrap();
                client.get_system_info_blocking().unwrap();
            }
            drop(client);
            peer.join().unwrap();
        }
    }
}

#[tokio::test]
async fn reconnect_does_not_reuse_the_previous_connections_admission() {
    let (mut client, mut guest) = blocking_pair();
    let peer = std::thread::spawn(move || {
        handshake(&mut guest, 7);
        receive(&mut guest, MessageType::GetSystemInfoRequest);
        respond(&mut guest, MessageType::GetSystemInfoResponse, &[]);
    });
    client.get_system_info_blocking().unwrap();
    peer.join().unwrap();
    client.disconnect().await.unwrap();
    let (next, guest) = blocking_pair();
    assert!(!client.protocol_admitted);
    assert!(
        client
            .connect()
            .await
            .unwrap_err()
            .to_string()
            .contains("obtain a new VM socket")
    );
    client = next;
    let peer = std::thread::spawn(move || unary_peer(guest, 6, false));
    assert_rejected(client.get_system_info_blocking());
    drop(client);
    peer.join().unwrap();
}

#[cfg(target_os = "macos")]
mod asynchronous {
    use super::*;
    use std::future::Future;
    use std::task::Poll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn async_pair() -> (AgentClient, UnixStream) {
        let (host, guest) = UnixStream::pair().unwrap();
        (
            AgentClient::from_fd_async(3, host.into_raw_fd()).unwrap(),
            guest,
        )
    }

    #[tokio::test]
    async fn partial_ping_timeout_and_cancellation_close_the_connection() {
        for (public_ping, timeout) in [(false, true), (false, false), (true, false)] {
            let (mut client, guest) = async_pair();
            guest.set_nonblocking(true).unwrap();
            let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();
            let mut request = Box::pin(async {
                if public_ping {
                    client.ping().await.map(drop)
                } else {
                    client.get_system_info().await.map(drop)
                }
            });
            tokio::select! {
                result = &mut request => panic!("handshake completed before Pong: {result:?}"),
                () = async {
                    let mut length = [0; 4];
                    guest.read_exact(&mut length).await.unwrap();
                    let mut frame = vec![0; u32::from_be_bytes(length) as usize];
                    guest.read_exact(&mut frame).await.unwrap();
                    assert_eq!(u32::from_be_bytes(frame[..4].try_into().unwrap()), MessageType::PingRequest as u32);
                    guest.write_all(&[0, 0]).await.unwrap();
                } => {}
            }
            std::future::poll_fn(|cx| {
                assert!(request.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            if timeout {
                tokio::time::pause();
                tokio::time::advance(BLOCKING_RPC_TIMEOUT).await;
                assert!(
                    request
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("handshake timed out")
                );
                tokio::time::resume();
            } else {
                drop(request);
            }
            assert!(!client.connected);
            assert!(!client.protocol_admitted);
            assert!(matches!(
                client.transport.async_send(Bytes::new()).await,
                Err(arcbox_transport::error::TransportError::NotConnected)
            ));
            assert_eq!(guest.read(&mut [0]).await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn async_unary_admits_v7_once_and_rejects_v6_after_explicit_ping() {
        for version in [6, 7] {
            for explicit_ping in [false, true] {
                let (mut client, guest) = async_pair();
                let peer = std::thread::spawn(move || unary_peer(guest, version, explicit_ping));
                if explicit_ping {
                    assert_eq!(client.ping().await.unwrap().protocol_version, version);
                }
                if version == 6 {
                    assert_rejected(client.get_system_info().await);
                } else {
                    client.get_system_info().await.unwrap();
                    client.get_system_info().await.unwrap();
                }
                drop(client);
                peer.join().unwrap();
            }
        }
    }
}
