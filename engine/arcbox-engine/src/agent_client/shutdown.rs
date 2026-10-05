use arcbox_connect::v1::{ShutdownRequest, ShutdownResponse};
use arcbox_constants::wire::MessageType;
use buffa::Message as _;

use super::AgentClient;
use crate::error::{EngineError, Result};

impl AgentClient {
    /// Requests guest shutdown and waits for acceptance on a blocking connection.
    ///
    /// `timeout_seconds` sets the guest's process termination grace period.
    /// Zero selects the guest default. Protocol admission precedes the request.
    /// This method does not wait for the VM to stop.
    ///
    /// # Errors
    ///
    /// Returns an error if protocol admission or the RPC fails, or the guest
    /// rejects shutdown.
    pub fn shutdown_blocking(&mut self, timeout_seconds: u32) -> Result<()> {
        let request = ShutdownRequest {
            timeout_seconds,
            ..Default::default()
        };
        let response: ShutdownResponse = self.unary_rpc_blocking(
            MessageType::ShutdownRequest,
            &request.encode_to_vec(),
            MessageType::ShutdownResponse,
        )?;
        if !response.accepted {
            return Err(EngineError::Machine(
                "guest agent rejected shutdown".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use arcbox_connect::v1::AgentPingResponse;

    use super::*;
    use crate::agent_client::wire;

    fn receive(guest: &mut UnixStream, expected: MessageType) {
        let mut length = [0; 4];
        guest.read_exact(&mut length).unwrap();
        let mut frame = vec![0; u32::from_be_bytes(length) as usize];
        guest.read_exact(&mut frame).unwrap();
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().unwrap()),
            expected as u32
        );
    }

    fn shutdown_with_response(kind: MessageType, payload: Vec<u8>) -> Result<()> {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let peer = std::thread::spawn(move || {
            receive(&mut guest, MessageType::PingRequest);
            guest
                .write_all(&wire::build_message(
                    MessageType::PingResponse,
                    "",
                    &AgentPingResponse {
                        protocol_version: 7,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
                .unwrap();
            receive(&mut guest, MessageType::ShutdownRequest);
            guest
                .write_all(&wire::build_message(kind, "", &payload))
                .unwrap();
        });
        let result = AgentClient::from_fd_blocking(17, host.into_raw_fd())
            .unwrap()
            .shutdown_blocking(4);
        peer.join().unwrap();
        result
    }

    #[test]
    fn shutdown_requires_guest_acceptance() {
        for accepted in [false, true] {
            let result = shutdown_with_response(
                MessageType::ShutdownResponse,
                ShutdownResponse {
                    accepted,
                    ..Default::default()
                }
                .encode_to_vec(),
            );
            if accepted {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("guest agent rejected shutdown")
                );
            }
        }
    }

    #[test]
    fn shutdown_preserves_guest_error() {
        let message = "shutdown is unavailable";
        let mut payload = 503_i32.to_be_bytes().to_vec();
        payload.extend_from_slice(&(message.len() as u32).to_be_bytes());
        payload.extend_from_slice(message.as_bytes());
        assert!(matches!(
            shutdown_with_response(MessageType::Error, payload),
            Err(EngineError::Agent { code: 503, message: actual }) if actual == message
        ));
    }

    #[test]
    fn shutdown_rejects_other_response_types() {
        let error = shutdown_with_response(MessageType::PingResponse, Vec::new()).unwrap_err();
        assert!(error.to_string().contains("unexpected response type"));
    }
}
