//! The host's window on a sandbox stream (`SandboxStreamWindow` in
//! agent.proto).
//!
//! A streaming handler sends its frames through [`Windowed`], which lets
//! them out only as the host has room for them — counted in encoded payload
//! bytes, opening at `SANDBOX_STREAM_WINDOW` and refilled by the window the
//! host returns as its consumer takes frames. The host can therefore keep
//! reading the connection however slow that consumer is, and a guest→host
//! vsock the host stops reading is what stalls a whole VM on
//! Virtualization.framework.

use anyhow::{Context as _, ensure};
use arcbox_connect::v1::SandboxStreamWindow;
use arcbox_constants::wire::SANDBOX_STREAM_WINDOW;
use buffa::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::rpc::{MessageType, read_message, write_message};

/// A stream's connection, written within the host's window.
pub(super) struct Windowed<'a, S> {
    stream: &'a mut S,
    /// Encoded payload bytes the host still has room for.
    available: usize,
}

impl<'a, S: AsyncRead + AsyncWrite + Unpin> Windowed<'a, S> {
    /// Takes over `stream` for the rest of a streaming request.
    pub(super) fn new(stream: &'a mut S) -> Self {
        Self {
            stream,
            available: SANDBOX_STREAM_WINDOW as usize,
        }
    }

    /// Writes a frame once the host has room for its payload. While
    /// waiting it reads the connection, where the only frame the host sends
    /// during a stream is returned window; anything else, or the host
    /// closing, ends the stream.
    pub(super) async fn write(
        &mut self,
        msg_type: MessageType,
        trace_id: &str,
        payload: &[u8],
    ) -> anyhow::Result<()> {
        ensure!(
            payload.len() <= SANDBOX_STREAM_WINDOW as usize,
            "a {msg_type:?} payload of {} bytes cannot fit the {SANDBOX_STREAM_WINDOW}-byte stream window",
            payload.len(),
        );
        while self.available < payload.len() {
            let (frame_type, _, frame) = read_message(self.stream)
                .await
                .context("waiting for the host to return stream window")?;
            ensure!(
                frame_type == MessageType::SandboxStreamWindow,
                "unexpected {frame_type:?} frame during a sandbox stream"
            );
            let grant = SandboxStreamWindow::decode_from_slice(&frame)
                .context("decoding a stream window frame")?;
            let total = self.available + grant.bytes as usize;
            ensure!(
                total <= SANDBOX_STREAM_WINDOW as usize,
                "host returned more stream window than it was given"
            );
            self.available = total;
        }
        self.available -= payload.len();
        write_message(self.stream, msg_type, trace_id, payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut as _;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt as _;

    const WINDOW: usize = SANDBOX_STREAM_WINDOW as usize;

    fn window_frame(bytes: u32) -> Vec<u8> {
        let payload = SandboxStreamWindow {
            bytes,
            ..Default::default()
        }
        .encode_to_vec();
        let mut buf = Vec::new();
        buf.put_u32((4 + 2 + payload.len()) as u32);
        buf.put_u32(MessageType::SandboxStreamWindow as u32);
        buf.put_u16(0);
        buf.extend_from_slice(&payload);
        buf
    }

    #[tokio::test]
    async fn frames_wait_for_the_window_the_host_returns() {
        let (mut guest, mut host) = tokio::io::duplex(WINDOW * 4);
        let mut windowed = Windowed::new(&mut guest);
        let payload = vec![0u8; WINDOW / 2];
        windowed
            .write(MessageType::SandboxFileData, "", &payload)
            .await
            .unwrap();
        windowed
            .write(MessageType::SandboxFileData, "", &payload)
            .await
            .unwrap();

        let blocked = tokio::time::timeout(
            Duration::from_millis(20),
            windowed.write(MessageType::SandboxFileData, "", &payload[..1]),
        )
        .await;
        assert!(blocked.is_err(), "the window is used up");

        host.write_all(&window_frame(1)).await.unwrap();
        windowed
            .write(MessageType::SandboxFileData, "", &payload[..1])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_host_that_returns_too_much_or_something_else_ends_the_stream() {
        let (mut guest, mut host) = tokio::io::duplex(WINDOW * 4);
        let mut windowed = Windowed::new(&mut guest);
        windowed
            .write(MessageType::SandboxFileData, "", &vec![0u8; WINDOW])
            .await
            .unwrap();
        host.write_all(&window_frame(1)).await.unwrap();
        host.write_all(&window_frame(WINDOW as u32)).await.unwrap();
        let err = windowed
            .write(MessageType::SandboxFileData, "", &[0u8; 2])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("more stream window"), "{err}");

        let (mut guest, mut host) = tokio::io::duplex(WINDOW * 4);
        let mut windowed = Windowed::new(&mut guest);
        windowed
            .write(MessageType::SandboxFileData, "", &vec![0u8; WINDOW])
            .await
            .unwrap();
        let mut ping = Vec::new();
        ping.put_u32(6);
        ping.put_u32(MessageType::PingRequest as u32);
        ping.put_u16(0);
        host.write_all(&ping).await.unwrap();
        let err = windowed
            .write(MessageType::SandboxFileData, "", &[0u8; 1])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unexpected"), "{err}");
    }

    #[tokio::test]
    async fn a_payload_larger_than_the_window_is_refused_not_waited_for() {
        let (mut guest, _host) = tokio::io::duplex(64);
        let mut windowed = Windowed::new(&mut guest);
        let err = windowed
            .write(MessageType::SandboxFileData, "", &vec![0u8; WINDOW + 1])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot fit"), "{err}");
    }
}
