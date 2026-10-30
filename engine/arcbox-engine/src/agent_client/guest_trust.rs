//! Current public-root delivery over a dedicated, cancellable agent connection.

use arcbox_connect::v1::{
    ReconcileGuestTrustRequest, ReconcileGuestTrustResponse,
    reconcile_guest_trust_request::Desired, reconcile_guest_trust_response::Applied,
};
use arcbox_constants::wire::MessageType;
use buffa::Message as _;
use tokio_util::sync::CancellationToken;

use super::AgentClient;
use crate::error::{EngineError, Result};

impl AgentClient {
    /// Applies an explicit client trust state through either guest transport.
    /// Await completion after cancellation. Compare the fingerprint and VM incarnation
    /// with the current desired state before reporting success to the user.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or the guest acknowledges a different operation.
    pub async fn reconcile_guest_trust(
        self,
        desired: Desired,
        cancelled: &CancellationToken,
    ) -> Result<Applied> {
        let removing = matches!(&desired, Desired::Remove(_));
        let payload = ReconcileGuestTrustRequest {
            desired: Some(desired),
            ..Default::default()
        }
        .encode_to_vec();
        let blocking_payload = payload.clone();
        let response: ReconcileGuestTrustResponse = self
            .cancellable(
                cancelled,
                move |mut client| {
                    client.unary_rpc_blocking(
                        MessageType::ReconcileGuestTrustRequest,
                        &blocking_payload,
                        MessageType::ReconcileGuestTrustResponse,
                    )
                },
                move |mut client| async move {
                    client
                        .unary_rpc(
                            MessageType::ReconcileGuestTrustRequest,
                            &payload,
                            MessageType::ReconcileGuestTrustResponse,
                        )
                        .await
                },
            )
            .await?;
        let applied = response.applied.ok_or_else(|| {
            EngineError::Machine("guest trust response has no applied state".into())
        })?;
        if matches!(&applied, Applied::Removed(_)) != removing {
            return Err(EngineError::Machine(
                "guest trust response acknowledges a different operation".into(),
            ));
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use arcbox_connect::v1::AgentPingResponse;
    use arcbox_constants::wire::AGENT_PROTOCOL_VERSION;

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

    fn admit(guest: &mut UnixStream, desired: &Desired) {
        receive(guest, MessageType::PingRequest);
        guest
            .write_all(&wire::build_message(
                MessageType::PingResponse,
                "",
                &AgentPingResponse {
                    protocol_version: AGENT_PROTOCOL_VERSION,
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
            .unwrap();
        let payload = receive(guest, MessageType::ReconcileGuestTrustRequest);
        let request = ReconcileGuestTrustRequest::decode_from_slice(&payload).unwrap();
        assert_eq!(request.desired.as_ref(), Some(desired));
    }

    fn desired(installing: bool) -> Desired {
        if installing {
            Desired::CertificatePem(
                b"-----BEGIN CERTIFICATE-----\ntransport fixture\n-----END CERTIFICATE-----\n"
                    .to_vec(),
            )
        } else {
            Desired::Remove(Default::default())
        }
    }

    fn connection(blocking: bool) -> (AgentClient, UnixStream) {
        let (host, guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let client = if blocking {
            AgentClient::from_fd_blocking(3, host.into_raw_fd())
        } else {
            AgentClient::from_fd_async(3, host.into_raw_fd())
        };
        (client.unwrap(), guest)
    }

    #[tokio::test]
    async fn trust_acknowledgement_requires_the_requested_operation() {
        for blocking in [false, true] {
            for installing in [false, true] {
                for installed in [None, Some(true), Some(false)] {
                    let (client, mut guest) = connection(blocking);
                    let applied = installed.map(|installed| {
                        if installed {
                            Applied::InstalledSha256("A1:B2".into())
                        } else {
                            Applied::Removed(Default::default())
                        }
                    });
                    let response = ReconcileGuestTrustResponse {
                        applied: applied.clone(),
                        ..Default::default()
                    }
                    .encode_to_vec();
                    let peer = std::thread::spawn(move || {
                        admit(&mut guest, &desired(installing));
                        guest
                            .write_all(&wire::build_message(
                                MessageType::ReconcileGuestTrustResponse,
                                "",
                                &response,
                            ))
                            .unwrap();
                    });
                    let result = client
                        .reconcile_guest_trust(desired(installing), &CancellationToken::new())
                        .await;
                    peer.join().unwrap();
                    assert_eq!(result.is_ok(), installed == Some(installing));
                    if let Ok(acknowledged) = result {
                        assert_eq!(Some(acknowledged), applied);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn cancelling_a_trust_update_closes_the_pending_acknowledgement() {
        for blocking in [false, true] {
            for installing in [false, true] {
                let (client, mut guest) = connection(blocking);
                let (started, ready) = tokio::sync::oneshot::channel();
                let peer = std::thread::spawn(move || {
                    admit(&mut guest, &desired(installing));
                    started.send(()).unwrap();
                    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
                });
                let cancelled = CancellationToken::new();
                let operation_cancelled = cancelled.clone();
                let operation = tokio::spawn(async move {
                    client
                        .reconcile_guest_trust(desired(installing), &operation_cancelled)
                        .await
                });
                ready.await.unwrap();
                cancelled.cancel();
                let result = tokio::time::timeout(Duration::from_secs(2), operation)
                    .await
                    .unwrap()
                    .unwrap();
                peer.join().unwrap();
                assert!(result.unwrap_err().to_string().contains("cancelled"));
            }
        }
    }
}
