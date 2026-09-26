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
