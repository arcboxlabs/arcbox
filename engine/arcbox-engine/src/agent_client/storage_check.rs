//! Dedicated, cancellable storage verification RPC.

use std::time::{Duration, Instant};

use arcbox_connect::v1::{StorageCheckRequest, StorageCheckResponse};
use arcbox_constants::wire::MessageType;
use arcbox_transport::Transport as _;
use buffa::Message as _;

use super::transport::{AgentTransport, BLOCKING_RPC_TIMEOUT};
use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

// Each offline checker has a 30-minute budget. Reserve one minute for RPC
// framing and process termination after both sequential checks.
const CHECK_TIMEOUT: Duration = Duration::from_secs(61 * 60);

impl AgentClient {
    /// Checks storage on a dedicated connection. Dropping the future closes
    /// the connection and cancels an offline checker in the recovery guest.
    ///
    /// # Errors
    /// Returns transport, deadline, protocol, and guest errors.
    pub async fn storage_check(
        mut self,
        request: &StorageCheckRequest,
    ) -> Result<StorageCheckResponse> {
        if !self.connected {
            tokio::time::timeout(BLOCKING_RPC_TIMEOUT, self.connect())
                .await
                .map_err(|_| failure("connection timed out"))??;
        }
        if let AgentTransport::Blocking(transport) = &self.transport {
            // Keep this guard outside the blocking task so cancellation also
            // interrupts a pending protocol handshake or checker receive.
            let _shutdown = transport.shutdown_handle().map_err(failure)?;
            let request = request.clone();
            return tokio::task::spawn_blocking(move || self.storage_check_blocking(&request))
                .await
                .map_err(failure)?;
        }
        self.require_agent_protocol().await?;
        let AgentTransport::Async(mut transport) = self.transport else {
            unreachable!("blocking transport returned above");
        };
        let message = wire::build_message(
            MessageType::StorageCheckRequest,
            "",
            &request.encode_to_vec(),
        );
        tokio::time::timeout(BLOCKING_RPC_TIMEOUT, transport.send(message))
            .await
            .map_err(|_| failure("send timed out"))?
            .map_err(failure)?;
        let raw = tokio::time::timeout(CHECK_TIMEOUT, transport.recv())
            .await
            .map_err(|_| failure("request timed out"))?
            .map_err(failure)?;
        decode(&raw)
    }

    /// Checks storage synchronously over an HV connection. The caller must
    /// stop the recovery VM to interrupt a pending check on this path.
    ///
    /// # Errors
    /// Returns transport, deadline, protocol, and guest errors.
    pub fn storage_check_blocking(
        mut self,
        request: &StorageCheckRequest,
    ) -> Result<StorageCheckResponse> {
        self.require_agent_protocol_blocking()?;
        let AgentTransport::Blocking(mut transport) = self.transport else {
            return Err(failure("blocking check requires the blocking transport"));
        };
        let message = wire::build_message(
            MessageType::StorageCheckRequest,
            "",
            &request.encode_to_vec(),
        );
        transport
            .send(&message, Instant::now() + BLOCKING_RPC_TIMEOUT)
            .map_err(failure)?;
        let raw = transport
            .recv(Instant::now() + CHECK_TIMEOUT)
            .map_err(failure)?;
        decode(&raw)
    }
}

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("storage check: {error}"))
}

fn decode(raw: &[u8]) -> Result<StorageCheckResponse> {
    let (kind, _, payload) = wire::parse_response(raw)?;
    if kind == MessageType::Error as u32 {
        let (code, message) = wire::parse_error_response(&payload)?;
        return Err(EngineError::Agent { code, message });
    }
    if kind != MessageType::StorageCheckResponse as u32 {
        return Err(failure(format!("unexpected response type 0x{kind:04x}")));
    }
    StorageCheckResponse::decode_from_slice(&payload).map_err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_connect::v1::AgentPingResponse;
    use std::io::{Read as _, Write as _};
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;

    fn connected_client() -> (AgentClient, UnixStream) {
        let (host, guest) = UnixStream::pair().unwrap();
        // SAFETY: ownership of this connected socket passes to the transport.
        let transport = unsafe {
            arcbox_transport::vsock::BlockingVsockTransport::from_raw_fd(host.into_raw_fd())
                .unwrap()
        };
        (
            AgentClient {
                cid: 3,
                transport: AgentTransport::Blocking(transport),
                connected: true,
                protocol_admitted: false,
            },
            guest,
        )
    }

    fn read_request(guest: &mut UnixStream, kind: MessageType) {
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut length = [0; 4];
        guest.read_exact(&mut length).unwrap();
        let mut request = vec![0; u32::from_be_bytes(length) as usize];
        guest.read_exact(&mut request).unwrap();
        assert_eq!(
            u32::from_be_bytes(request[..4].try_into().unwrap()),
            kind as u32
        );
    }

    fn handshake(guest: &mut UnixStream, protocol_version: u32) {
        read_request(guest, MessageType::PingRequest);
        guest
            .write_all(&wire::build_message(
                MessageType::PingResponse,
                "",
                &AgentPingResponse {
                    protocol_version,
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn blocking_check_decodes_the_result() {
        let (client, mut guest) = connected_client();
        let server = std::thread::spawn(move || {
            handshake(&mut guest, 7);
            read_request(&mut guest, MessageType::StorageCheckRequest);
            guest
                .write_all(&wire::build_message(
                    MessageType::StorageCheckResponse,
                    "",
                    &StorageCheckResponse {
                        passed: true,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ))
                .unwrap();
        });
        assert!(
            client
                .storage_check(&StorageCheckRequest::default())
                .await
                .unwrap()
                .passed
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_blocking_check_closes_the_socket() {
        let (client, mut guest) = connected_client();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            handshake(&mut guest, 7);
            read_request(&mut guest, MessageType::StorageCheckRequest);
            sent.send(()).unwrap();
            assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
        });
        let check =
            tokio::spawn(
                async move { client.storage_check(&StorageCheckRequest::default()).await },
            );
        received.await.unwrap();
        check.abort();
        assert!(check.await.unwrap_err().is_cancelled());
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn token_cancellation_finishes_a_blocking_check_before_returning() {
        let (client, mut guest) = connected_client();
        let cancelled = tokio_util::sync::CancellationToken::new();
        let requested_cancel = cancelled.clone();
        let server = std::thread::spawn(move || {
            handshake(&mut guest, 7);
            read_request(&mut guest, MessageType::StorageCheckRequest);
            requested_cancel.cancel();
            assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
        });
        let error = client
            .storage_check_with_cancel(StorageCheckRequest::default(), &cancelled)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        server.join().unwrap();
    }

    #[test]
    fn old_agents_cannot_receive_storage_or_runtime_start_requests() {
        let requests: [fn(AgentClient) -> Result<()>; 3] = [
            |client| {
                client
                    .storage_check_blocking(&StorageCheckRequest::default())
                    .map(drop)
            },
            |mut client| client.ensure_runtime_blocking(true).map(drop),
            |client| {
                client
                    .watch_readiness_blocking(true, Duration::from_secs(1), "test")
                    .map(drop)
            },
        ];
        for request in requests {
            let (client, mut guest) = connected_client();
            let server = std::thread::spawn(move || {
                handshake(&mut guest, 6);
                assert_eq!(
                    guest.read(&mut [0; 1]).unwrap(),
                    0,
                    "protocol 6 must not receive a storage mutation request"
                );
            });
            let error = request(client).unwrap_err();
            assert!(error.to_string().contains("daemon requires >= 7"));
            server.join().unwrap();
        }
        let error = AgentClient::check_agent_protocol(&AgentPingResponse {
            protocol_version: 6,
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.to_string().contains("daemon requires >= 7"));
    }

    #[cfg(target_os = "macos")]
    mod asynchronous {
        use super::*;

        fn async_client() -> (AgentClient, UnixStream) {
            let (host, guest) = UnixStream::pair().unwrap();
            let client = AgentClient::from_fd_async(3, host.into_raw_fd()).unwrap();
            assert!(matches!(client.transport, AgentTransport::Async(_)));
            (client, guest)
        }

        #[tokio::test]
        async fn async_checks_require_v7_and_preserve_the_check_result() {
            for (version, passed) in [(6, false), (7, false), (7, true)] {
                let (client, mut guest) = async_client();
                let server = std::thread::spawn(move || {
                    handshake(&mut guest, version);
                    if version == 7 {
                        read_request(&mut guest, MessageType::StorageCheckRequest);
                        guest
                            .write_all(&wire::build_message(
                                MessageType::StorageCheckResponse,
                                "",
                                &StorageCheckResponse {
                                    passed,
                                    ..Default::default()
                                }
                                .encode_to_vec(),
                            ))
                            .unwrap();
                    }
                    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
                });
                let result = client.storage_check(&StorageCheckRequest::default()).await;
                if version == 6 {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("daemon requires >= 7")
                    );
                } else {
                    assert_eq!(result.unwrap().passed, passed);
                }
                server.join().unwrap();
            }
        }

        #[tokio::test]
        async fn async_checks_preserve_guest_errors_and_reject_wrong_frames() {
            for kind in [MessageType::Error, MessageType::PingResponse] {
                let (client, mut guest) = async_client();
                let server = std::thread::spawn(move || {
                    handshake(&mut guest, 7);
                    read_request(&mut guest, MessageType::StorageCheckRequest);
                    let mut payload = 412_i32.to_be_bytes().to_vec();
                    payload.extend_from_slice(&4_u32.to_be_bytes());
                    payload.extend_from_slice(b"held");
                    guest
                        .write_all(&wire::build_message(kind, "", &payload))
                        .unwrap();
                    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
                });
                let error = client
                    .storage_check(&StorageCheckRequest::default())
                    .await
                    .unwrap_err();
                if kind == MessageType::Error {
                    assert!(
                        matches!(error, EngineError::Agent { code: 412, message } if message == "held")
                    );
                } else {
                    assert!(error.to_string().contains("unexpected response type"));
                }
                server.join().unwrap();
            }
        }

        #[tokio::test]
        async fn cancellation_and_deadline_close_pending_checks() {
            for (blocking, handshake_complete, expire) in [
                (true, false, false),
                (false, false, false),
                (false, true, false),
                (false, true, true),
            ] {
                let (client, mut guest) = if blocking {
                    connected_client()
                } else {
                    async_client()
                };
                let (sent, received) = tokio::sync::oneshot::channel();
                let server = std::thread::spawn(move || {
                    if handshake_complete {
                        handshake(&mut guest, 7);
                        read_request(&mut guest, MessageType::StorageCheckRequest);
                    } else {
                        read_request(&mut guest, MessageType::PingRequest);
                    }
                    sent.send(()).unwrap();
                    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
                });
                let check = tokio::spawn(async move {
                    client.storage_check(&StorageCheckRequest::default()).await
                });
                received.await.unwrap();
                if expire {
                    tokio::time::pause();
                    // Tokio rounds timer deadlines up to its next millisecond tick.
                    tokio::time::advance(CHECK_TIMEOUT + Duration::from_millis(1)).await;
                    assert!(
                        check
                            .await
                            .unwrap()
                            .unwrap_err()
                            .to_string()
                            .contains("request timed out")
                    );
                    tokio::time::resume();
                } else {
                    check.abort();
                    assert!(check.await.unwrap_err().is_cancelled());
                }
                tokio::task::spawn_blocking(move || server.join().unwrap())
                    .await
                    .unwrap();
            }
        }
    }
}
