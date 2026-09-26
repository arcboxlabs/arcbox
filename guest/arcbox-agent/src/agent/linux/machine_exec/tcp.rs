//! TCP connections from inside the machine (`MachineTcpConnectRequest`, SSH
//! `direct-tcpip`), carried like a flow-controlled session: host stdin goes
//! to the peer, the peer's bytes come back as output, and each side's
//! half-close travels on its own.

use std::time::Duration;

use anyhow::Context as _;
use arcbox_connect::v1::MachineTcpConnectRequest;
use buffa::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::flow::Flow;
use super::session::{self, Ended, OUTPUT_CHANNEL_CAPACITY, Streams};
use crate::rpc::ErrorResponse;

/// How long a connect may take before the host is told it failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects as the request asks and relays until both directions are
/// closed or the host goes away; the relay owns the rest of the connection.
pub(in crate::agent::linux) async fn handle_tcp_connect<S>(
    stream: &mut S,
    trace_id: &str,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = MachineTcpConnectRequest::decode_from_slice(payload)
        .context("failed to decode MachineTcpConnectRequest")?;
    let flow = match Flow::new(req.output_window) {
        Ok(flow) if flow.initial_stdin_window().is_some() => flow,
        Ok(_) => {
            let err = ErrorResponse::new(400, "output_window is required".to_owned());
            return session::write_error(stream, trace_id, &err).await;
        }
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    let socket = match connect(&req).await {
        Ok(socket) => socket,
        Err(err) => return session::write_error(stream, trace_id, &err).await,
    };
    if let Some(window) = flow.initial_stdin_window() {
        session::write_window(stream, trace_id, window).await?;
    }

    let (peer_rd, peer_wr) = socket.into_split();
    let (output_tx, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let reader = session::read_output(peer_rd, "stdout", output_tx);
    let (stdin, stdin_rx) = mpsc::unbounded_channel();
    let (delivered_tx, delivered) = mpsc::unbounded_channel();
    // Ends once the host has ended its stdin and the sending side is shut.
    let writer = tokio::spawn(session::write_stdin(Some(peer_wr), stdin_rx, delivered_tx));
    let stop_writer = writer.abort_handle();
    let closed = async {
        writer.await.context("tcp writer task failed")?;
        Ok(Ended::Closed)
    };

    let streams = Streams {
        output,
        stdin,
        delivered,
    };
    let finished = session::relay(stream, trace_id, &flow, streams, None, None, closed).await;
    // Either way the socket goes: a peer that never sends again must not
    // keep it open.
    reader.abort();
    stop_writer.abort();
    finished.map(drop)
}

/// Opens the connection, or the error the host reports for it.
async fn connect(req: &MachineTcpConnectRequest) -> Result<TcpStream, ErrorResponse> {
    let target = format!("{}:{}", req.host, req.port);
    let port = u16::try_from(req.port)
        .map_err(|_| ErrorResponse::new(400, format!("{target}: port out of range")))?;
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((req.host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(socket)) => Ok(socket),
        Ok(Err(e)) => Err(ErrorResponse::new(503, format!("connect to {target}: {e}"))),
        Err(_) => Err(ErrorResponse::new(
            504,
            format!("connect to {target}: timed out"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use arcbox_connect::v1::{MachineExecOutput, MachineExecWindow};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
    use tokio::net::TcpListener;

    use super::*;
    use crate::rpc::{MessageType, read_message, write_message};

    fn request(port: u16) -> Vec<u8> {
        MachineTcpConnectRequest {
            host: "127.0.0.1".to_owned(),
            port: port.into(),
            output_window: 64 * 1024,
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// Starts a relay whose host side is the returned stream.
    fn relay(port: u16) -> (DuplexStream, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let (host, mut agent) = tokio::io::duplex(64 * 1024);
        let task =
            tokio::spawn(async move { handle_tcp_connect(&mut agent, "", &request(port)).await });
        (host, task)
    }

    async fn next(host: &mut DuplexStream) -> (MessageType, Vec<u8>) {
        let (msg_type, _, payload) = read_message(host).await.unwrap();
        (msg_type, payload)
    }

    async fn next_output(host: &mut DuplexStream) -> MachineExecOutput {
        let (msg_type, payload) = next(host).await;
        assert_eq!(msg_type, MessageType::MachineExecOutput);
        MachineExecOutput::decode_from_slice(&payload).unwrap()
    }

    #[tokio::test]
    async fn relays_both_ways_and_carries_each_half_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // A peer that answers, stops sending, and then reads until the
        // host stops sending too.
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"hello").await.unwrap();
            socket.shutdown().await.unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).await.unwrap();
            received
        });
        let (mut host, relay) = relay(port);

        assert_eq!(next(&mut host).await.0, MessageType::MachineExecInputWindow);
        assert_eq!(next_output(&mut host).await.data, b"hello");
        assert!(next_output(&mut host).await.eof, "the peer's half-close");

        write_message(&mut host, MessageType::MachineExecInput, "", b"world")
            .await
            .unwrap();
        write_message(&mut host, MessageType::MachineExecInput, "", b"")
            .await
            .unwrap();
        assert_eq!(peer.await.unwrap(), b"world");

        // Both directions closed: the final frame, maybe after the stdin
        // window comes back.
        loop {
            let (msg_type, payload) = next(&mut host).await;
            if msg_type == MessageType::MachineExecInputWindow {
                let returned = MachineExecWindow::decode_from_slice(&payload).unwrap();
                assert_eq!(returned.bytes, 5);
                continue;
            }
            assert_eq!(msg_type, MessageType::MachineExecOutput);
            assert!(MachineExecOutput::decode_from_slice(&payload).unwrap().done);
            break;
        }
        relay.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_refused_connection_is_the_error_frame() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let (mut host, relay) = relay(port);

        let (msg_type, _) = next(&mut host).await;
        assert_eq!(msg_type, MessageType::Error);
        relay.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_host_leaving_closes_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (mut host, relay) = relay(port);
        let (mut socket, _) = listener.accept().await.unwrap();
        assert_eq!(next(&mut host).await.0, MessageType::MachineExecInputWindow);

        drop(host);
        relay.await.unwrap().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(socket.read(&mut buf).await.unwrap(), 0, "the relay hung up");
    }
}
