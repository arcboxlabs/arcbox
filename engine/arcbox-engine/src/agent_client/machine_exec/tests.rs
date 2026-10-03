//! Machine exec sessions against a fake guest on the far end of a
//! socketpair — the fd shape VZ hands the async transport.

use std::os::fd::IntoRawFd;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::*;

/// Stdin window the fake guest grants unless a test says otherwise.
const STDIN_WINDOW: u32 = 64 * 1024;

fn session_pair() -> (AgentClient, UnixStream) {
    let (host, guest) = std::os::unix::net::UnixStream::pair().unwrap();
    guest.set_nonblocking(true).unwrap();
    let client = AgentClient::from_fd_async(3, host.into_raw_fd()).unwrap();
    (client, UnixStream::from_std(guest).unwrap())
}

/// A session whose guest granted `stdin_window`, with the request frame
/// already read off the guest side.
async fn start(
    stdin_window: u32,
) -> (
    ExecSessionOutput,
    mpsc::Sender<ExecSessionInput>,
    UnixStream,
) {
    let (client, mut guest) = session_pair();
    write_window(&mut guest, stdin_window).await;
    let (input, input_rx) = mpsc::channel(4);
    let output = client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
        .unwrap();
    let (msg_type, payload) = read_frame(&mut guest).await;
    assert_eq!(msg_type, MessageType::MachineExecRequest as u32);
    let request = MachineExecRequest::decode_from_slice(&payload).unwrap();
    assert_eq!(request.output_window, OUTPUT_WINDOW);
    (output, input, guest)
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

/// Writes an output frame and returns the window it takes.
async fn write_output(guest: &mut UnixStream, output: MachineExecOutput) -> usize {
    let payload = output.encode_to_vec();
    let frame = wire::build_message(MessageType::MachineExecOutput, "", &payload);
    guest.write_all(&frame).await.unwrap();
    payload.len()
}

async fn write_window(guest: &mut UnixStream, bytes: u32) {
    let window = MachineExecWindow {
        bytes,
        ..Default::default()
    };
    let frame = wire::build_message(
        MessageType::MachineExecInputWindow,
        "",
        &window.encode_to_vec(),
    );
    guest.write_all(&frame).await.unwrap();
}

async fn write_error(guest: &mut UnixStream, code: i32, message: &str) {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(&(message.len() as u32).to_be_bytes());
    payload.extend_from_slice(message.as_bytes());
    let frame = wire::build_message(MessageType::Error, "", &payload);
    guest.write_all(&frame).await.unwrap();
}

fn data(len: usize) -> MachineExecOutput {
    MachineExecOutput {
        data: vec![b'x'; len],
        ..Default::default()
    }
}

#[tokio::test]
async fn session_forwards_input_and_ends_at_the_final_output() {
    let (mut output, input, mut guest) = start(STDIN_WINDOW).await;

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

    let exit = MachineExecOutput {
        done: true,
        exit_code: -1,
        exit_signal: "KILL".to_owned(),
        ..Default::default()
    };
    write_output(&mut guest, data(3)).await;
    write_output(&mut guest, exit).await;
    assert_eq!(output.recv().await.unwrap().unwrap().data, b"xxx");
    assert_eq!(output.recv().await.unwrap().unwrap().exit_signal, "KILL");
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn exec_without_input_sends_stdin_eof() {
    let (client, mut guest) = session_pair();
    write_window(&mut guest, STDIN_WINDOW).await;
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
async fn dropping_the_output_closes_the_connection() {
    // Input stays open: only the consumer going away may end the session.
    let (output, _input, mut guest) = start(STDIN_WINDOW).await;

    drop(output);
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), guest.read(&mut buf))
        .await
        .expect("the connection must close without further guest output");
    assert_eq!(read.unwrap(), 0);
}

#[tokio::test]
async fn an_agent_error_frame_ends_the_session_with_that_error() {
    let (mut output, _input, mut guest) = start(STDIN_WINDOW).await;

    write_error(&mut guest, 404, "no such").await;
    match output.recv().await.unwrap() {
        Err(EngineError::Agent { code, message }) => {
            assert_eq!((code, message.as_str()), (404, "no such"));
        }
        other => panic!("expected the agent error, got {other:?}"),
    }
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn a_request_the_agent_refuses_fails_to_start() {
    let (client, mut guest) = session_pair();
    write_error(&mut guest, 400, "bad user").await;
    let (_input, input_rx) = mpsc::channel(1);
    match client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
    {
        Err(EngineError::Agent { code, message }) => {
            assert_eq!((code, message.as_str()), (400, "bad user"));
        }
        Ok(_) => panic!("the session must not start"),
        Err(other) => panic!("expected the agent error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_agent_without_flow_control_is_refused() {
    let (client, mut guest) = session_pair();
    write_output(&mut guest, data(1)).await;
    let (_input, input_rx) = mpsc::channel(1);
    let err = client
        .machine_exec_session(MachineExecRequest::default(), input_rx)
        .await
        .err()
        .expect("an agent that streams at once predates flow control");
    assert!(err.to_string().contains("restart the machine"), "{err}");
}

#[tokio::test]
async fn stdin_waits_for_the_window_the_agent_grants() {
    let (_output, input, mut guest) = start(4).await;
    input
        .send(ExecSessionInput::Stdin(b"hello".to_vec()))
        .await
        .unwrap();

    let stdin = MessageType::MachineExecInput as u32;
    assert_eq!(read_frame(&mut guest).await, (stdin, b"hell".to_vec()));
    let early = tokio::time::timeout(Duration::from_millis(50), read_frame(&mut guest)).await;
    assert!(early.is_err(), "no stdin may go out past the window");

    write_window(&mut guest, 4).await;
    assert_eq!(read_frame(&mut guest).await, (stdin, b"o".to_vec()));
}

#[tokio::test]
async fn output_is_drained_ahead_of_the_consumer_and_window_returned_as_it_takes_it() {
    let (mut output, _input, mut guest) = start(STDIN_WINDOW).await;

    // A whole window of output goes through while nobody takes any: the
    // connection is drained, not left full.
    let frame_data = 32 * 1024;
    let mut sent = Vec::new();
    let mut total = 0;
    while total + write_cost(frame_data) <= OUTPUT_WINDOW as usize {
        let cost = tokio::time::timeout(
            Duration::from_secs(5),
            write_output(&mut guest, data(frame_data)),
        )
        .await
        .expect("the host must keep reading while its consumer does not");
        total += cost;
        sent.push(cost);
    }

    let mut taken = 0;
    for cost in &sent {
        output.recv().await.unwrap().unwrap();
        taken += cost;
        if taken >= OUTPUT_WINDOW as usize / 2 {
            break;
        }
    }
    let (msg_type, payload) = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut guest))
        .await
        .expect("taking half the window returns it");
    assert_eq!(msg_type, MessageType::MachineExecOutputWindow as u32);
    let returned = MachineExecWindow::decode_from_slice(&payload)
        .unwrap()
        .bytes;
    assert_eq!(returned as usize, taken);
}

#[tokio::test]
async fn an_agent_that_overruns_the_output_window_ends_the_session() {
    let (mut output, _input, mut guest) = start(STDIN_WINDOW).await;

    let frame_data = 32 * 1024;
    let mut total = 0;
    while total <= OUTPUT_WINDOW as usize {
        total += write_output(&mut guest, data(frame_data)).await;
    }
    // The host cuts the session off at the frame that overran — before its
    // consumer took anything that would have returned window...
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), guest.read(&mut buf))
        .await
        .expect("the host must close the connection");
    assert_eq!(read.unwrap(), 0);

    // ...and the consumer learns why after the output that was in window.
    let err = loop {
        match output
            .recv()
            .await
            .expect("the session reports the overrun")
        {
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("overran"), "{err}");
    assert!(output.recv().await.is_none());
}

fn write_cost(len: usize) -> usize {
    data(len).encode_to_vec().len()
}

#[tokio::test]
async fn a_tcp_connection_asks_for_its_peer_and_streams_like_a_session() {
    let (client, mut guest) = session_pair();
    write_window(&mut guest, STDIN_WINDOW).await;
    let (input, input_rx) = mpsc::channel(4);
    let mut output = client
        .machine_tcp_connect("localhost", 8080, input_rx)
        .await
        .unwrap();

    let (msg_type, payload) = read_frame(&mut guest).await;
    assert_eq!(msg_type, MessageType::MachineTcpConnectRequest as u32);
    let request = MachineTcpConnectRequest::decode_from_slice(&payload).unwrap();
    assert_eq!(
        (request.host.as_str(), request.port, request.output_window),
        ("localhost", 8080, OUTPUT_WINDOW)
    );

    input
        .send(ExecSessionInput::Stdin(b"GET /".to_vec()))
        .await
        .unwrap();
    let stdin = MessageType::MachineExecInput as u32;
    assert_eq!(read_frame(&mut guest).await, (stdin, b"GET /".to_vec()));
    write_output(&mut guest, data(2)).await;
    let eof = MachineExecOutput {
        eof: true,
        ..Default::default()
    };
    write_output(&mut guest, eof).await;
    assert_eq!(output.recv().await.unwrap().unwrap().data, b"xx");
    assert!(output.recv().await.unwrap().unwrap().eof);
}

#[tokio::test]
async fn a_refused_tcp_connection_fails_to_open() {
    let (client, mut guest) = session_pair();
    write_error(&mut guest, 503, "connect to localhost:1: refused").await;
    let (_input, input_rx) = mpsc::channel(1);
    match client.machine_tcp_connect("localhost", 1, input_rx).await {
        Err(EngineError::Agent { code, .. }) => assert_eq!(code, 503),
        Ok(_) => panic!("the connection must not open"),
        Err(other) => panic!("expected the agent error, got {other:?}"),
    }
}
