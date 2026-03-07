//! Host-side port-forward control over vsock:54 (`FWD_PORT`).
//!
//! The host initiates forwarding by sending `FWD_START {port}` on vsock:54;
//! vm-agent opens a vsock listener on `{port}` and acknowledges.  Subsequent
//! host connections to vsock:`{port}` are bridged by vm-agent to TCP
//! `127.0.0.1:{port}` inside the sandbox.
//!
//! ## Protocol (vsock:54)
//!
//! Frame format: `[u8 type][u32 LE len][payload]` — same as all other channels.
//! One connection per control request.
//!
//! | Hex  | Name        | Direction    | Payload              |
//! |------|-------------|--------------|----------------------|
//! | 0x40 | `FWD_START` | Host → Agent | JSON `{"port": u16}` |
//! | 0x41 | `FWD_STOP`  | Host → Agent | JSON `{"port": u16}` |
//! | 0x50 | `FWD_ACK`   | Agent → Host | empty                |
//! | 0x51 | `FWD_ERR`   | Agent → Host | UTF-8 error message  |
//!
//! ## Data flow (after FWD_START succeeds)
//!
//! ```text
//! Host  →  vsock:{port}  CONNECT
//!          vm-agent accepts, bridges to TCP 127.0.0.1:{port}
//!          ←═══════ bidirectional pipe ═══════▶
//! ```
//!
//! ## Security
//!
//! Only the host can issue `FWD_START` (vsock is host-initiated).  vm-agent
//! rejects ports < 1024 to avoid accidentally forwarding privileged services.

use std::path::Path;

use serde::Serialize;

use crate::error::{Result, VmmError};
use crate::vsock::{connect_to_port, read_frame, write_frame};

/// Guest-side vsock port for the port-forward control channel.
pub const FWD_PORT: u32 = 54;

// Control frame types.
const FWD_START: u8 = 0x40;
const FWD_STOP: u8 = 0x41;
const FWD_ACK: u8 = 0x50;
const FWD_ERR: u8 = 0x51;

#[derive(Serialize)]
struct FwdReq {
    port: u16,
}

async fn send_fwd_cmd(uds_path: &Path, cmd: u8, port: u16) -> Result<()> {
    let mut stream = connect_to_port(uds_path, FWD_PORT).await?;

    let req = serde_json::to_vec(&FwdReq { port })
        .map_err(|e| VmmError::Vsock(format!("serialize FwdReq: {e}")))?;
    write_frame(&mut stream, cmd, &req)
        .await
        .map_err(|e| VmmError::Vsock(format!("write fwd cmd: {e}")))?;

    let (resp_type, payload) = read_frame(&mut stream)
        .await
        .map_err(|e| VmmError::Vsock(format!("read fwd response: {e}")))?;

    match resp_type {
        FWD_ACK => Ok(()),
        FWD_ERR => Err(VmmError::Vsock(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        other => Err(VmmError::Vsock(format!(
            "port forward: unexpected response type 0x{other:02x}"
        ))),
    }
}

/// Tell vm-agent to start forwarding vsock:`port` → TCP `127.0.0.1:{port}`.
///
/// After this returns, the host can connect to `vsock:{port}` on the
/// sandbox's UDS path and get a bidirectional pipe to the in-sandbox TCP
/// service on the same port.
///
/// Returns an error if the port is already forwarded or the agent rejected it.
pub async fn start_forward(uds_path: &Path, port: u16) -> Result<()> {
    send_fwd_cmd(uds_path, FWD_START, port).await
}

/// Tell vm-agent to stop forwarding `port`.
///
/// In-flight connections are drained before the vsock listener is torn down.
/// Returns an error if the port was not being forwarded.
pub async fn stop_forward(uds_path: &Path, port: u16) -> Result<()> {
    send_fwd_cmd(uds_path, FWD_STOP, port).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fwd_req_serializes() {
        let req = FwdReq { port: 8080 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("8080"));
    }
}
