//! Guest-owned web route snapshots over both agent transports.

use arcbox_connect::v1::{
    ListGuestWebServicesRequest, ListGuestWebServicesResponse, ReconcileHttpsAccessRequest,
    ReconcileHttpsAccessResponse,
};
use arcbox_constants::wire::MessageType;
use buffa::Message;
use tokio_util::sync::CancellationToken;

use super::AgentClient;
use crate::error::Result;

impl AgentClient {
    /// Renews managed HTTPS observation on a dedicated, cancellable connection.
    /// The operation owner must await this future after cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails.
    pub async fn reconcile_https_access(
        self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        let payload = request.encode_to_vec();
        let blocking_payload = payload.clone();
        self.cancellable(
            cancelled,
            move |mut client| {
                client.unary_rpc_blocking(
                    MessageType::ReconcileHttpsAccessRequest,
                    &blocking_payload,
                    MessageType::ReconcileHttpsAccessResponse,
                )
            },
            move |mut client| async move {
                client
                    .unary_rpc(
                        MessageType::ReconcileHttpsAccessRequest,
                        &payload,
                        MessageType::ReconcileHttpsAccessResponse,
                    )
                    .await
            },
        )
        .await
    }

    /// Lists guest web routes on a dedicated connection through either transport.
    /// The operation owner must await this future after cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails or is cancelled.
    pub async fn list_guest_web_services(
        self,
        request: ListGuestWebServicesRequest,
        cancelled: &CancellationToken,
    ) -> Result<ListGuestWebServicesResponse> {
        let payload = request.encode_to_vec();
        let blocking_payload = payload.clone();
        self.cancellable(
            cancelled,
            move |mut client| {
                client.unary_rpc_blocking(
                    MessageType::ListGuestWebServicesRequest,
                    &blocking_payload,
                    MessageType::ListGuestWebServicesResponse,
                )
            },
            move |mut client| async move {
                client
                    .unary_rpc(
                        MessageType::ListGuestWebServicesRequest,
                        &payload,
                        MessageType::ListGuestWebServicesResponse,
                    )
                    .await
            },
        )
        .await
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

    fn receive(guest: &mut UnixStream, expected: MessageType) -> Vec<u8> {
        let mut length = [0; 4];
        guest.read_exact(&mut length).unwrap();
        let mut frame = vec![0; u32::from_be_bytes(length) as usize];
        guest.read_exact(&mut frame).unwrap();
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().unwrap()),
            expected as u32
        );
        let trace_length = u16::from_be_bytes(frame[4..6].try_into().unwrap()) as usize;
        frame[6 + trace_length..].to_vec()
    }

    #[tokio::test]
    async fn cancelled_https_observation_closes_handshake_and_business_reads() {
        for admitted in [false, true] {
            let (host, mut guest) = UnixStream::pair().unwrap();
            guest
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (started, ready) = tokio::sync::oneshot::channel();
            let peer = std::thread::spawn(move || {
                drop(receive(&mut guest, MessageType::PingRequest));
                if admitted {
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
                    drop(receive(
                        &mut guest,
                        MessageType::ReconcileHttpsAccessRequest,
                    ));
                }
                started.send(()).unwrap();
                assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
            });
            let client = AgentClient::from_fd_blocking(3, host.into_raw_fd()).unwrap();
            let cancelled = CancellationToken::new();
            let operation_cancelled = cancelled.clone();
            let operation = tokio::spawn(async move {
                client
                    .reconcile_https_access(
                        ReconcileHttpsAccessRequest {
                            observation_id: "cycle".into(),
                            enabled: true,
                            host_addresses: vec!["192.168.64.1".into()],
                            ..Default::default()
                        },
                        &operation_cancelled,
                    )
                    .await
            });
            ready.await.unwrap();
            cancelled.cancel();
            let result = tokio::time::timeout(Duration::from_secs(2), operation)
                .await
                .unwrap()
                .unwrap();
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            peer.join().unwrap();
        }
    }
    #[tokio::test]
    async fn directory_requests_preserve_the_hostname_and_join_cancelled_reads() {
        for blocking in [false, true] {
            for reply in [false, true] {
                let (host, mut guest) = UnixStream::pair().unwrap();
                guest
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let (started, ready) = tokio::sync::oneshot::channel();
                let peer = std::thread::spawn(move || {
                    drop(receive(&mut guest, MessageType::PingRequest));
                    guest
                        .write_all(&wire::build_message(
                            MessageType::PingResponse,
                            "",
                            &AgentPingResponse {
                                protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                                ..Default::default()
                            }
                            .encode_to_vec(),
                        ))
                        .unwrap();
                    let payload = receive(&mut guest, MessageType::ListGuestWebServicesRequest);
                    let request = ListGuestWebServicesRequest::decode_from_slice(&payload).unwrap();
                    assert_eq!(request.hostname, "dev.arcbox.local");
                    started.send(()).unwrap();
                    if reply {
                        guest
                            .write_all(&wire::build_message(
                                MessageType::ListGuestWebServicesResponse,
                                "",
                                &ListGuestWebServicesResponse::default().encode_to_vec(),
                            ))
                            .unwrap();
                    }
                    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
                });
                let client = if blocking {
                    AgentClient::from_fd_blocking(3, host.into_raw_fd())
                } else {
                    AgentClient::from_fd_async(3, host.into_raw_fd())
                }
                .unwrap();
                let cancelled = CancellationToken::new();
                let operation_cancelled = cancelled.clone();
                let operation = tokio::spawn(async move {
                    client
                        .list_guest_web_services(
                            ListGuestWebServicesRequest {
                                hostname: "dev.arcbox.local".into(),
                                ..Default::default()
                            },
                            &operation_cancelled,
                        )
                        .await
                });
                ready.await.unwrap();
                if !reply {
                    cancelled.cancel();
                }
                let result = tokio::time::timeout(Duration::from_secs(2), operation)
                    .await
                    .unwrap()
                    .unwrap();
                if reply {
                    assert_eq!(result.unwrap(), ListGuestWebServicesResponse::default());
                } else {
                    assert!(result.unwrap_err().to_string().contains("cancelled"));
                }
                peer.join().unwrap();
            }
        }
    }
}
