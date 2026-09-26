//! Machine exec sessions against a fake guest on the far end of a
//! socketpair — the fd shape VZ hands the async transport.

use std::os::fd::IntoRawFd;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

async fn write_output(guest: &mut UnixStream, output: MachineExecOutput) {
    let frame = wire::build_message(MessageType::MachineExecOutput, "", &output.encode_to_vec());
    guest.write_all(&frame).await.unwrap();
}

#[tokio::test]
async fn session_forwards_input_and_ends_at_the_final_output() {
    let (client, mut guest) = session_pair();
    let (input, input_rx) = mpsc::channel(4);
    let mut output = client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
        .unwrap();
    assert_eq!(
        read_frame(&mut guest).await.0,
        MessageType::MachineExecRequest as u32
    );

    input
        .send(ExecSessionInput::Stdin(b"hi".to_vec()))
        .await
        .unwrap();
    input
        .send(ExecSessionInput::Stdin(Vec::new()))
        .await
        .unwrap();
    input
        .send(ExecSessionInput::Signal("INT".to_owned()))
        .await
        .unwrap();
    let stdin = MessageType::MachineExecInput as u32;
    assert_eq!(read_frame(&mut guest).await, (stdin, b"hi".to_vec()));
    // EOF closes stdin but not the input stream: the signal still goes.
    assert_eq!(read_frame(&mut guest).await, (stdin, Vec::new()));
    let (msg_type, payload) = read_frame(&mut guest).await;
    assert_eq!(msg_type, MessageType::MachineExecSignal as u32);
    assert_eq!(
        MachineExecSignal::decode_from_slice(&payload).unwrap().name,
        "INT"
    );

    let data = MachineExecOutput {
        data: b"out".to_vec(),
        ..Default::default()
    };
    let exit = MachineExecOutput {
        done: true,
        exit_code: -1,
        exit_signal: "KILL".to_owned(),
        ..Default::default()
    };
    write_output(&mut guest, data).await;
    write_output(&mut guest, exit).await;
    assert_eq!(output.recv().await.unwrap().unwrap().data, b"out");
    assert_eq!(output.recv().await.unwrap().unwrap().exit_signal, "KILL");
    assert!(output.recv().await.is_none());
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

#[tokio::test]
async fn an_agent_error_frame_ends_the_session_with_that_error() {
    let (client, mut guest) = session_pair();
    let (_input, input_rx) = mpsc::channel(1);
    let mut output = client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
        .unwrap();
    read_frame(&mut guest).await;

    let mut payload = 404_i32.to_be_bytes().to_vec();
    payload.extend_from_slice(&7_u32.to_be_bytes());
    payload.extend_from_slice(b"no such");
    let frame = wire::build_message(MessageType::Error, "", &payload);
    guest.write_all(&frame).await.unwrap();

    match output.recv().await.unwrap() {
        Err(EngineError::Agent { code, message }) => {
            assert_eq!((code, message.as_str()), (404, "no such"));
        }
        other => panic!("expected the agent error, got {other:?}"),
    }
    assert!(output.recv().await.is_none());
}
