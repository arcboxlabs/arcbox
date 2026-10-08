//! Guest-owned web route snapshots over both agent transports.

use arcbox_connect::v1::{
    ListContainerWebServicesRequest, ListContainerWebServicesResponse, ReconcileHttpsAccessRequest,
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

    /// Lists installed container web routes and their current DNS aliases.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails.
    pub async fn list_container_web_services(
        &mut self,
        request: ListContainerWebServicesRequest,
    ) -> Result<ListContainerWebServicesResponse> {
        self.unary_rpc(
            MessageType::ListContainerWebServicesRequest,
            &request.encode_to_vec(),
            MessageType::ListContainerWebServicesResponse,
        )
        .await
    }

    /// Lists container web routes on the HV socketpair. Call from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails.
    pub fn list_container_web_services_blocking(
        &mut self,
        request: ListContainerWebServicesRequest,
    ) -> Result<ListContainerWebServicesResponse> {
        self.unary_rpc_blocking(
            MessageType::ListContainerWebServicesRequest,
            &request.encode_to_vec(),
            MessageType::ListContainerWebServicesResponse,
        )
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

    #[tokio::test]
    async fn cancelled_https_observation_closes_handshake_and_business_reads() {
        for admitted in [false, true] {
            let (host, mut guest) = UnixStream::pair().unwrap();
            guest
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (started, ready) = tokio::sync::oneshot::channel();
            let peer = std::thread::spawn(move || {
                receive(&mut guest, MessageType::PingRequest);
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
                    receive(&mut guest, MessageType::ReconcileHttpsAccessRequest);
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
}
