//! The agent frame protocol the sandbox messages ride.
//!
//! The same length-prefixed V2 frames the System VM's agent serves on vsock
//! port 1024 (`arcbox_constants::wire`): the host reaches this service over
//! that vsock for the nested location and over an in-process socket for the
//! host location, with the same client either way.

use std::io::Cursor;

use anyhow::{Context, Result};
use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub use arcbox_constants::wire::MessageType;

/// The payload of an `Error` frame: an HTTP-style code and a message.
#[derive(Debug, Clone)]
pub struct ErrorResponse {
    pub code: i32,
    pub message: String,
}

impl ErrorResponse {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.put_i32(self.code);
        let msg_bytes = self.message.as_bytes();
        buf.put_u32(u32::try_from(msg_bytes.len()).unwrap_or(u32::MAX));
        buf.extend_from_slice(msg_bytes);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        if data.len() < 8 {
            anyhow::bail!("error response too short");
        }
        let code = cursor.get_i32();
        let msg_len = cursor.get_u32() as usize;
        if data.len() < 8 + msg_len {
            anyhow::bail!("error response message truncated");
        }
        let message = String::from_utf8(data[8..8 + msg_len].to_vec())?;
        Ok(Self { code, message })
    }
}

/// Reads a single RPC message from the stream.
///
/// Wire format V2:
/// ```text
/// +----------------+----------------+------------------+----------------+
/// | Length (4B BE) | Type (4B BE)   | TraceLen (2B BE) | TraceID bytes  | Payload
/// +----------------+----------------+------------------+----------------+
/// ```
/// Length = sizeof(Type) + sizeof(TraceLen) + TraceLen + PayloadLen
///        = 4 + 2 + TraceLen + PayloadLen
///
/// Returns (message_type, trace_id, payload).
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(MessageType, String, Vec<u8>)> {
    let mut frame = arcbox_transport::vsock::read_framed(reader)
        .await
        .context("failed to read RPC frame")?;
    frame.advance(4);
    anyhow::ensure!(
        frame.remaining() >= 6,
        "RPC frame is shorter than V2 header"
    );
    let msg_type_raw = frame.get_u32();
    let msg_type = MessageType::from_u32(msg_type_raw)
        .with_context(|| format!("unknown message type: {msg_type_raw}"))?;
    let trace_len = usize::from(frame.get_u16());
    anyhow::ensure!(trace_len <= frame.remaining(), "trace ID exceeds RPC frame");
    let trace_id = std::str::from_utf8(&frame.split_to(trace_len))
        .context("trace ID is not UTF-8")?
        .to_owned();
    Ok((msg_type, trace_id, frame.to_vec()))
}

/// Writes a single RPC message to the stream.
///
/// Wire format V2 (see `read_message` for layout).
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    trace_id: &str,
    payload: &[u8],
) -> Result<()> {
    let trace_bytes = trace_id.as_bytes();
    let trace_len = trace_bytes.len().min(u16::MAX as usize);

    // Length = type(4) + trace_len_field(2) + trace_bytes + payload
    let length = 4 + 2 + trace_len + payload.len();

    let mut buf = BytesMut::with_capacity(8 + 2 + trace_len + payload.len());
    buf.put_u32(u32::try_from(length).context("RPC frame exceeds 4 GiB")?);
    buf.put_u32(msg_type as u32);
    buf.put_u16(u16::try_from(trace_len).unwrap_or(u16::MAX));
    if trace_len > 0 {
        buf.extend_from_slice(&trace_bytes[..trace_len]);
    }
    buf.extend_from_slice(payload);

    // Name the frame in the error: on a torn-down connection the resulting
    // "Connection closed by peer" warn must say which response/stream write
    // was lost, or the failure cannot be attributed (CORE-82).
    writer
        .write_all(&buf)
        .await
        .with_context(|| format!("failed to write {msg_type:?} message"))?;
    writer
        .flush()
        .await
        .with_context(|| format!("failed to flush {msg_type:?} message"))?;

    Ok(())
}
