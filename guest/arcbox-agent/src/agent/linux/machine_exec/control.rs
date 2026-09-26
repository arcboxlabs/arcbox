//! Host frames on a running session's connection: stdin, terminal resizes,
//! signals and returned output window.
//!
//! The reader runs concurrently with the output pump and never waits on the
//! process, so a stalled stdin consumer never stops output, and output never
//! stops the reader from noticing that the host went away.

use buffa::Message as _;
use nix::sys::signal::Signal;
use tokio::io::AsyncRead;

use crate::rpc::{MessageType, read_message};

/// One host instruction for the running process.
pub(super) enum Control {
    /// Bytes for the process's stdin (never empty).
    Stdin(Vec<u8>),
    /// The host's stdin ended.
    Eof,
    /// The host terminal changed size.
    Resize(arcbox_pty::WinSize),
    /// A signal for the process.
    Signal(Signal),
    /// Output window the host returned (`flow.rs`).
    OutputWindow(u32),
}

/// Reads the next instruction; `None` once the host has closed the
/// connection or broken the protocol, which ends the session.
///
/// Not cancel-safe — a frame read half-way is lost — so a session keeps one
/// reader loop alive for its whole life instead of racing a fresh call
/// against output in a `select!` loop.
pub(super) async fn next<R: AsyncRead + Unpin>(conn: &mut R) -> Option<Control> {
    loop {
        let (msg_type, _, payload) = match read_message(conn).await {
            Ok(frame) => frame,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "machine exec host connection ended");
                return None;
            }
        };
        match msg_type {
            MessageType::MachineExecInput if payload.is_empty() => return Some(Control::Eof),
            MessageType::MachineExecInput => return Some(Control::Stdin(payload)),
            MessageType::MachineExecResize => {
                match arcbox_connect::v1::TerminalSize::decode_from_slice(&payload) {
                    Ok(size) => {
                        return Some(Control::Resize(arcbox_pty::WinSize {
                            cols: u16::try_from(size.width).unwrap_or(u16::MAX),
                            rows: u16::try_from(size.height).unwrap_or(u16::MAX),
                        }));
                    }
                    Err(e) => tracing::warn!(error = %e, "bad machine exec resize frame"),
                }
            }
            MessageType::MachineExecSignal => {
                match arcbox_connect::v1::MachineExecSignal::decode_from_slice(&payload) {
                    Ok(frame) => match format!("SIG{}", frame.name).parse::<Signal>() {
                        Ok(signal) => return Some(Control::Signal(signal)),
                        Err(_) => {
                            tracing::warn!(name = %frame.name, "unknown signal for machine exec");
                        }
                    },
                    Err(e) => tracing::warn!(error = %e, "bad machine exec signal frame"),
                }
            }
            MessageType::MachineExecOutputWindow => {
                match arcbox_connect::v1::MachineExecWindow::decode_from_slice(&payload) {
                    Ok(frame) => return Some(Control::OutputWindow(frame.bytes)),
                    // A lost grant would stall output for good: end the
                    // session instead.
                    Err(e) => {
                        tracing::warn!(error = %e, "bad machine exec window frame");
                        return None;
                    }
                }
            }
            other => tracing::warn!(?other, "unexpected frame during machine exec session"),
        }
    }
}
