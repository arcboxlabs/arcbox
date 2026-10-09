//! Dispatches profile-local CA mutations to the daemon's single TLS worker.

use std::sync::{Arc, OnceLock};

use arcbox_connect::v1 as pb;
use connectrpc::ConnectError;
use tokio::sync::{mpsc, oneshot};

/// The worker completes admitted mutations even when the reply receiver disconnects.
#[derive(Debug)]
pub enum LocalCaCommand {
    Prepare {
        expected_current_sha256: String,
        reply: oneshot::Sender<Result<pb::LocalCaRotation, ConnectError>>,
    },
    Commit {
        expected_old_sha256: String,
        expected_new_sha256: String,
        reply: oneshot::Sender<Result<pb::Empty, ConnectError>>,
    },
}

/// Shared registration permits RPCs before the VM runtime is available.
#[derive(Clone, Debug, Default)]
pub struct LocalCaControl {
    sender: Arc<OnceLock<mpsc::Sender<LocalCaCommand>>>,
}

impl LocalCaControl {
    /// Registers one worker with space for one command waiting behind the active mutation.
    pub fn register(&self) -> Result<mpsc::Receiver<LocalCaCommand>, ConnectError> {
        let (sender, receiver) = mpsc::channel(1);
        self.sender.set(sender).map_err(|_| {
            ConnectError::already_exists("the local CA worker is already registered")
        })?;
        Ok(receiver)
    }

    pub(super) async fn prepare(
        &self,
        request: pb::PrepareLocalCaRotationRequest,
    ) -> Result<pb::LocalCaRotation, ConnectError> {
        self.request(|reply| LocalCaCommand::Prepare {
            expected_current_sha256: request.expected_current_sha256,
            reply,
        })
        .await
    }

    pub(super) async fn commit(
        &self,
        request: pb::CommitLocalCaRotationRequest,
    ) -> Result<pb::Empty, ConnectError> {
        self.request(|reply| LocalCaCommand::Commit {
            expected_old_sha256: request.expected_old_sha256,
            expected_new_sha256: request.expected_new_sha256,
            reply,
        })
        .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ConnectError>>) -> LocalCaCommand,
    ) -> Result<T, ConnectError> {
        let sender = self
            .sender
            .get()
            .ok_or_else(|| ConnectError::unavailable("the local CA worker is not running"))?;
        let (reply, response) = oneshot::channel();
        sender
            .try_send(command(reply))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ConnectError::resource_exhausted(
                    "the local CA worker already has a waiting command",
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    ConnectError::unavailable("the local CA worker has stopped")
                }
            })?;
        response.await.map_err(|_| {
            ConnectError::unavailable(
                "the local CA worker did not confirm the operation; inspect HTTPS status before retrying",
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_connect::v1::SystemService as _;
    use buffa::Message as _;
    use connectrpc::{ErrorCode, HasMessageView, RequestContext, ServiceRequest};

    use crate::{SetupState, SystemServiceImpl};

    #[tokio::test]
    async fn unavailable_and_full_workers_reject_commands_without_waiting() {
        let control = LocalCaControl::default();
        let error = control.prepare(Default::default()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);

        let receiver = control.clone().register().unwrap();
        assert_eq!(
            control.register().unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        let (reply, _) = oneshot::channel();
        control
            .sender
            .get()
            .unwrap()
            .try_send(LocalCaCommand::Prepare {
                expected_current_sha256: "current".into(),
                reply,
            })
            .unwrap();
        let error = control.commit(Default::default()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceExhausted);

        drop(receiver);
        let error = control.prepare(Default::default()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn interrupted_worker_does_not_report_a_completed_mutation() {
        let control = LocalCaControl::default();
        let mut receiver = control.register().unwrap();
        let (result, ()) = tokio::join!(control.commit(Default::default()), async {
            drop(receiver.recv().await.unwrap());
        });
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
        assert!(
            error
                .message
                .as_deref()
                .unwrap()
                .contains("did not confirm")
        );
    }

    #[tokio::test]
    async fn system_handlers_dispatch_pinned_identities_without_a_ready_runtime() {
        let setup = Arc::new(SetupState::new());
        let mut receiver = setup.local_ca.register().unwrap();
        let service = SystemServiceImpl::new(setup, Arc::default(), Arc::default(), Arc::default());
        let bytes = bytes::Bytes::from(
            pb::PrepareLocalCaRotationRequest {
                expected_current_sha256: "previous".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        let view = pb::PrepareLocalCaRotationRequest::decode_view(&bytes).unwrap();
        let (response, ()) = tokio::join!(
            service.prepare_local_ca_rotation(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&view, &bytes),
            ),
            async {
                let LocalCaCommand::Prepare {
                    expected_current_sha256,
                    reply,
                } = receiver.recv().await.unwrap()
                else {
                    panic!("expected a prepare command");
                };
                assert_eq!(expected_current_sha256, "previous");
                reply
                    .send(Ok(pb::LocalCaRotation {
                        previous_certificate_der: vec![1],
                        candidate_certificate_der: vec![2],
                        ..Default::default()
                    }))
                    .unwrap();
            }
        );
        let rotation = response.unwrap().body;
        assert_eq!(rotation.previous_certificate_der, [1]);
        assert_eq!(rotation.candidate_certificate_der, [2]);

        let bytes = bytes::Bytes::from(
            pb::CommitLocalCaRotationRequest {
                expected_old_sha256: "previous".into(),
                expected_new_sha256: "candidate".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        let view = pb::CommitLocalCaRotationRequest::decode_view(&bytes).unwrap();
        let (response, ()) = tokio::join!(
            service.commit_local_ca_rotation(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&view, &bytes),
            ),
            async {
                let LocalCaCommand::Commit {
                    expected_old_sha256,
                    expected_new_sha256,
                    reply,
                } = receiver.recv().await.unwrap()
                else {
                    panic!("expected a commit command");
                };
                assert_eq!(expected_old_sha256, "previous");
                assert_eq!(expected_new_sha256, "candidate");
                reply
                    .send(Err(ConnectError::failed_precondition(
                        "candidate is not trusted",
                    )))
                    .unwrap();
            }
        );
        let error = response.unwrap_err();
        assert_eq!(error.code, ErrorCode::FailedPrecondition);
        assert_eq!(error.message.as_deref(), Some("candidate is not trusted"));
    }
}
