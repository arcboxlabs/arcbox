//! Runs pinned CA mutations under the profile lock before publishing status and replying.

use arcbox_api::{LocalCaCommand, SetupState};
use arcbox_connect::v1::{Empty, LocalCaRotation};
use connectrpc::ConnectError;

use super::*;

pub(super) async fn execute(
    dir: &Path,
    authority: &watch::Sender<Option<Arc<KeychainAuthority>>>,
    state: &SetupState,
    command: LocalCaCommand,
) {
    match command {
        LocalCaCommand::Prepare {
            expected_current_sha256,
            reply,
        } => {
            let result = mutate(dir, state, move |dir| {
                let rotation = KeychainAuthority::prepare_rotation(dir, &expected_current_sha256)?;
                Ok(LocalCaRotation {
                    previous_certificate_der: rotation.previous.der().to_vec(),
                    candidate_certificate_der: rotation.candidate.der().to_vec(),
                    ..Default::default()
                })
            })
            .await;
            // A disconnected client does not cancel an admitted mutation.
            let _ = reply.send(result);
        }
        LocalCaCommand::Commit {
            expected_old_sha256,
            expected_new_sha256,
            reply,
        } => {
            let authority = authority.clone();
            let result = mutate(dir, state, move |dir| {
                let mut active = authority.send_replace(None);
                let committed = KeychainAuthority::commit_rotation(
                    dir,
                    &expected_old_sha256,
                    &expected_new_sha256,
                )
                .map(|authority| (Arc::new(authority), Publication::Published))
                .map_err(Into::into);
                let result = reconcile(&mut active, committed, |authority| {
                    is_published(dir, authority)
                });
                authority.send_replace(active);
                result?;
                Ok(Empty::default())
            })
            .await;
            let _ = reply.send(result);
        }
    }
}

async fn mutate<T: Send + 'static>(
    dir: &Path,
    state: &SetupState,
    apply: impl FnOnce(&Path) -> Result<T> + Send + 'static,
) -> Result<T, ConnectError> {
    let task = tokio::task::spawn_blocking({
        let dir = dir.to_owned();
        move || match arcbox_local_ca::lock_profile(&dir) {
            Ok(operation) => (apply(&dir), Some(operation)),
            Err(error) => (Err(error.into()), None),
        }
    })
    .await;
    let (result, _operation) = match task {
        Ok(result) => result,
        Err(error) => (Err(error.into()), None),
    };
    let status = inspect(dir).await;
    state.set_local_https(status);
    result.map_err(|error| {
        let message = format!("{error:#}");
        match error.downcast_ref::<arcbox_local_ca::Error>() {
            Some(arcbox_local_ca::Error::OperationInProgress) => ConnectError::aborted(message),
            Some(arcbox_local_ca::Error::InvalidAuthority(_)) => {
                ConnectError::failed_precondition(message)
            }
            _ => ConnectError::internal(message),
        }
    })
}
