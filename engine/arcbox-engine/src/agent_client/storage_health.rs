//! Cancellable storage observations over either agent transport.

use std::time::{Duration, Instant};

use arcbox_connect::v1::{StorageHealth, WatchStorageHealthRequest};
use arcbox_constants::wire::MessageType;
use arcbox_transport::Transport as _;
use arcbox_transport::vsock::BlockingVsockShutdown;
use buffa::Message as _;
use tokio::sync::mpsc;

use super::transport::{AgentTransport, BLOCKING_RPC_TIMEOUT};
use super::{AgentClient, wire};
use crate::error::{EngineError, Result};

// The guest sends a heartbeat every 30 seconds even without a state change.
const FRAME_TIMEOUT: Duration = Duration::from_secs(45);

/// A storage snapshot stream. Dropping the stream closes the guest connection.
pub struct StorageHealthStream {
    frames: mpsc::Receiver<Result<StorageHealth>>,
    task: tokio::task::JoinHandle<()>,
    _blocking_shutdown: Option<BlockingVsockShutdown>,
}

impl StorageHealthStream {
    /// Returns the next observation, or `None` when the connection has ended.
    pub async fn recv(&mut self) -> Option<Result<StorageHealth>> {
        self.frames.recv().await
    }
}

impl Drop for StorageHealthStream {
    fn drop(&mut self) {
        self.task.abort();
        // The shutdown handle also interrupts a running blocking task, which
        // JoinHandle::abort alone cannot cancel.
    }
}

impl AgentClient {
    /// Opens a storage health watch without ensuring or starting the runtime.
    /// The first frame contains the current snapshot after protocol admission.
    pub async fn watch_storage_health(mut self) -> Result<StorageHealthStream> {
        if !self.connected {
            self.connect().await?;
        }
        let blocking_shutdown = match &self.transport {
            AgentTransport::Blocking(transport) => {
                Some(transport.shutdown_handle().map_err(failure)?)
            }
            AgentTransport::Async(_) => None,
        };
        if blocking_shutdown.is_some() {
            // Keep cancellation able to interrupt the blocking handshake before the watch exists.
            self = tokio::task::spawn_blocking(move || {
                self.require_agent_protocol_blocking()?;
                Ok::<_, EngineError>(self)
            })
            .await
            .map_err(failure)??;
        } else {
            self.require_agent_protocol().await?;
        }
        let request = wire::build_message(
            MessageType::WatchStorageHealthRequest,
            "",
            &WatchStorageHealthRequest::default().encode_to_vec(),
        );
        let (tx, frames) = mpsc::channel(1);
        let task = match self.transport {
            AgentTransport::Async(mut transport) => tokio::spawn(async move {
                let result = async {
                    tokio::time::timeout(BLOCKING_RPC_TIMEOUT, transport.send(request))
                        .await
                        .map_err(|_| failure("send timed out"))?
                        .map_err(failure)?;
                    loop {
                        let raw = tokio::time::timeout(FRAME_TIMEOUT, transport.recv())
                            .await
                            .map_err(|_| failure("heartbeat timed out"))?
                            .map_err(failure)?;
                        let snapshot = decode(&raw)?;
                        if tx.send(Ok(snapshot)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                .await;
                if let Err(error) = result {
                    let _ = tx.send(Err(error)).await;
                }
            }),
            AgentTransport::Blocking(mut transport) => tokio::task::spawn_blocking(move || {
                let result = (|| {
                    transport
                        .send(&request, Instant::now() + BLOCKING_RPC_TIMEOUT)
                        .map_err(failure)?;
                    loop {
                        let raw = transport
                            .recv(Instant::now() + FRAME_TIMEOUT)
                            .map_err(failure)?;
                        let snapshot = decode(&raw)?;
                        if tx.blocking_send(Ok(snapshot)).is_err() {
                            return Ok(());
                        }
                    }
                })();
                if let Err(error) = result {
                    let _ = tx.blocking_send(Err(error));
                }
            }),
        };
        Ok(StorageHealthStream {
            frames,
            task,
            _blocking_shutdown: blocking_shutdown,
        })
    }

    /// Reads runtime status on the HV blocking transport without starting services.
    pub fn get_runtime_status_blocking(
        &mut self,
    ) -> Result<arcbox_connect::v1::RuntimeStatusResponse> {
        self.unary_rpc_blocking(
            MessageType::RuntimeStatusRequest,
            &[],
            MessageType::RuntimeStatusResponse,
        )
    }
}

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Machine(format!("storage health watch: {error}"))
}

fn decode(raw: &[u8]) -> Result<StorageHealth> {
    let (kind, _, payload) = wire::parse_response(raw)?;
    if kind == MessageType::Error as u32 {
        let (code, message) = wire::parse_error_response(&payload)?;
        return Err(EngineError::Agent { code, message });
    }
    if kind != MessageType::StorageHealth as u32 {
        return Err(failure(format!("unexpected frame type 0x{kind:04x}")));
    }
    StorageHealth::decode_from_slice(&payload).map_err(failure)
}

#[cfg(all(test, target_os = "macos"))]
mod async_tests;

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
    async fn blocking_watch_delivers_snapshot_and_drop_closes_the_socket() {
        let (client, mut guest) = connected_client();
        let guest = std::thread::spawn(move || {
            handshake(&mut guest, arcbox_constants::wire::AGENT_PROTOCOL_VERSION);
            read_request(&mut guest, MessageType::WatchStorageHealthRequest);
            let snapshot = StorageHealth {
                observed_at_unix_ms: 123,
                ..Default::default()
            };
            guest
                .write_all(&wire::build_message(
                    MessageType::StorageHealth,
                    "",
                    &snapshot.encode_to_vec(),
                ))
                .unwrap();
            assert_eq!(
                guest.read(&mut [0; 1]).unwrap(),
                0,
                "drop must close a pending receive"
            );
        });
        let mut stream = client.watch_storage_health().await.unwrap();
        assert_eq!(
            stream.recv().await.unwrap().unwrap().observed_at_unix_ms,
            123
        );
        drop(stream);
        tokio::task::spawn_blocking(move || guest.join().unwrap())
            .await
            .unwrap();
    }

    #[test]
    fn blocking_runtime_requests_require_protocol_admission() {
        let requests: [fn(AgentClient) -> Result<()>; 5] = [
            |mut client| client.get_runtime_status_blocking().map(drop),
            |mut client| client.ensure_runtime_blocking(false).map(drop),
            |mut client| client.ensure_runtime_blocking(true).map(drop),
            |client| {
                client
                    .watch_readiness_blocking(false, Duration::from_secs(1), "test")
                    .map(drop)
            },
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
                    "incompatible agents must not receive the runtime request"
                );
            });
            assert!(
                request(client)
                    .unwrap_err()
                    .to_string()
                    .contains("daemon requires >= 7")
            );
            server.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn async_runtime_requests_require_protocol_admission() {
        for request in 0..6 {
            let (mut client, mut guest) = connected_client();
            let server = std::thread::spawn(move || {
                handshake(&mut guest, 6);
                assert_eq!(
                    guest.read(&mut [0; 1]).unwrap(),
                    0,
                    "incompatible agents must not receive the runtime request"
                );
            });
            let result = async move {
                match request {
                    0 => client.get_runtime_status().await.map(drop),
                    1 => client.ensure_runtime(false).await.map(drop),
                    2 => client
                        .watch_readiness(false, Duration::from_secs(1), "test")
                        .await
                        .map(drop),
                    3 => client.ensure_runtime(true).await.map(drop),
                    4 => client
                        .watch_readiness(true, Duration::from_secs(1), "test")
                        .await
                        .map(drop),
                    _ => client.watch_storage_health().await.map(drop),
                }
            }
            .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("daemon requires >= 7")
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_the_storage_handshake_closes_the_blocking_connection() {
        let (client, mut guest) = connected_client();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            read_request(&mut guest, MessageType::PingRequest);
            sent.send(()).unwrap();
            assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
        });
        let opening = tokio::spawn(client.watch_storage_health());
        received.await.unwrap();
        opening.abort();
        assert!(opening.await.err().unwrap().is_cancelled());
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }
}
