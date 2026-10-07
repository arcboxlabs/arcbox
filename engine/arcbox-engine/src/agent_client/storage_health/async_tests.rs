use std::os::fd::IntoRawFd as _;

use arcbox_connect::v1::AgentPingResponse;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

use super::*;

const IO_TIMEOUT: Duration = Duration::from_secs(2);

async fn read_request(guest: &mut UnixStream, expected: MessageType) {
    tokio::time::timeout(IO_TIMEOUT, async {
        let length = guest.read_u32().await.unwrap() as usize;
        let mut frame = vec![0; length];
        guest.read_exact(&mut frame).await.unwrap();
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().unwrap()),
            expected as u32
        );
    })
    .await
    .expect("the host must send the expected request");
}

async fn open_watch(version: u32) -> (Result<StorageHealthStream>, UnixStream) {
    let (host, guest) = std::os::unix::net::UnixStream::pair().unwrap();
    let client = AgentClient::from_fd_async(3, host.into_raw_fd()).unwrap();
    guest.set_nonblocking(true).unwrap();
    let mut guest = UnixStream::from_std(guest).unwrap();
    assert!(matches!(client.transport, AgentTransport::Async(_)));
    let (opening, ()) = tokio::join!(client.watch_storage_health(), async {
        read_request(&mut guest, MessageType::PingRequest).await;
        guest
            .write_all(&wire::build_message(
                MessageType::PingResponse,
                "",
                &AgentPingResponse {
                    protocol_version: version,
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
            .await
            .unwrap();
    });
    (opening, guest)
}

async fn assert_closed(guest: &mut UnixStream) {
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, guest.read(&mut [0; 1]))
            .await
            .expect("the watch must close the socket")
            .unwrap(),
        0,
        "the peer must receive EOF, not another request"
    );
}

async fn deliver_snapshot(stream: &mut StorageHealthStream, guest: &mut UnixStream) {
    read_request(guest, MessageType::WatchStorageHealthRequest).await;
    guest
        .write_all(&wire::build_message(
            MessageType::StorageHealth,
            "",
            &StorageHealth {
                observed_at_unix_ms: 123,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, stream.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .observed_at_unix_ms,
        123
    );
}

#[tokio::test]
async fn async_watch_rejects_v6_and_delivers_v7_until_drop() {
    for version in [6, 7] {
        let (opening, mut guest) = open_watch(version).await;
        if version == 6 {
            assert!(
                opening
                    .err()
                    .expect("v6 must not open a watch")
                    .to_string()
                    .contains("daemon requires >= 7")
            );
        } else {
            let mut stream = opening.unwrap();
            deliver_snapshot(&mut stream, &mut guest).await;
            drop(stream);
        }
        assert_closed(&mut guest).await;
    }
}

#[tokio::test]
async fn async_watch_delivers_guest_and_frame_errors_then_closes() {
    for kind in [MessageType::Error, MessageType::PingResponse] {
        let (opening, mut guest) = open_watch(7).await;
        let mut stream = opening.unwrap();
        read_request(&mut guest, MessageType::WatchStorageHealthRequest).await;
        let message = "inspection failed";
        let mut payload = 412_i32.to_be_bytes().to_vec();
        payload.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
        payload.extend_from_slice(message.as_bytes());
        guest
            .write_all(&wire::build_message(kind, "", &payload))
            .await
            .unwrap();
        let error = tokio::time::timeout(IO_TIMEOUT, stream.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        if kind == MessageType::Error {
            assert!(matches!(
                error,
                EngineError::Agent { code: 412, message: detail } if detail == message
            ));
        } else {
            assert!(
                error
                    .to_string()
                    .contains(&format!("unexpected frame type 0x{:04x}", kind as u32))
            );
        }
        assert!(
            tokio::time::timeout(IO_TIMEOUT, stream.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert_closed(&mut guest).await;
    }
}

#[tokio::test]
async fn async_watch_heartbeat_timeout_ends_the_stream_and_closes_the_socket() {
    let (opening, mut guest) = open_watch(7).await;
    let mut stream = opening.unwrap();
    // Receive a snapshot before pausing so socket readiness and the next receive timeout are established on the current-thread runtime.
    deliver_snapshot(&mut stream, &mut guest).await;
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    // Tokio rounds timer deadlines up to the next millisecond.
    let advance_by = FRAME_TIMEOUT + Duration::from_millis(1);
    tokio::time::advance(advance_by).await;
    let error = tokio::time::timeout(IO_TIMEOUT, stream.recv())
        .await
        .expect("the elapsed heartbeat deadline must reach the receiver")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("heartbeat timed out"));
    assert_eq!(start.elapsed(), advance_by);
    tokio::time::resume();
    assert!(
        tokio::time::timeout(IO_TIMEOUT, stream.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert_closed(&mut guest).await;
}
