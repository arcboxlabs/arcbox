//! Machine exec sessions against a fake guest on the far end of a
//! socketpair — the fd shape VZ hands the async transport.

use std::os::fd::IntoRawFd;

use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

use super::*;

fn session_pair() -> (AgentClient, UnixStream) {
    let (host, guest) = std::os::unix::net::UnixStream::pair().unwrap();
    guest.set_nonblocking(true).unwrap();
    let client = AgentClient::from_fd_async(3, host.into_raw_fd()).unwrap();
    (client, UnixStream::from_std(guest).unwrap())
}

/// One host frame as the guest reads it: `(type, payload)`.
async fn read_frame(guest: &mut UnixStream) -> (u32, Vec<u8>) {
    let mut header = [0u8; 8];
    guest.read_exact(&mut header).await.unwrap();
    let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let msg_type = u32::from_be_bytes(header[4..].try_into().unwrap());
    let mut rest = vec![0u8; len - 4];
    guest.read_exact(&mut rest).await.unwrap();
    let trace_len = u16::from_be_bytes(rest[..2].try_into().unwrap()) as usize;
    (msg_type, rest[2 + trace_len..].to_vec())
}

#[tokio::test]
async fn exec_without_input_sends_stdin_eof() {
    let (client, mut guest) = session_pair();
    let _output = client
        .machine_exec(MachineExecRequest::default())
        .await
        .unwrap();
    assert_eq!(
        read_frame(&mut guest).await.0,
        MessageType::MachineExecRequest as u32
    );
    assert_eq!(
        read_frame(&mut guest).await,
        (MessageType::MachineExecInput as u32, Vec::new())
    );
}

#[tokio::test]
async fn dropping_the_output_receiver_closes_the_connection() {
    let (client, mut guest) = session_pair();
    // Input stays open: only the consumer going away may end the session.
    let (_input, input_rx) = mpsc::channel(1);
    let output = client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
        .unwrap();
    read_frame(&mut guest).await;

    drop(output);
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), guest.read(&mut buf))
        .await
        .expect("the connection must close without further guest output");
    assert_eq!(read.unwrap(), 0);
}
