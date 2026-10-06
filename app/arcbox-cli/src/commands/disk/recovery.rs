//! Explicit storage operations with terminal-result semantics.

use anyhow::{Context, Result, bail};
use arcbox_connect::v1::{RecoverStorageRequest, StorageRecoveryProgress, SystemServiceClient};
use arcbox_connect::v1::{recover_storage_request::Action, storage_recovery_progress::Phase};

pub(super) async fn execute(action: Action) -> Result<()> {
    let (transport, config) = crate::connect::daemon(&super::super::resolve_grpc_socket_path());
    let client = SystemServiceClient::new(transport, config);
    let mut stream = client
        .recover_storage(RecoverStorageRequest {
            action: action.into(),
            ..Default::default()
        })
        .await
        .context("could not receive storage operation progress; inspect ArcBox storage status before retrying")?;

    let mut last: Option<StorageRecoveryProgress> = None;
    while let Some(message) = stream
        .message()
        .await
        .context("storage progress disconnected; the operation may continue in the daemon; inspect ArcBox storage status before retrying")?
    {
        let progress = message.to_owned_message();
        if last.is_none() {
            println!("Operation: {}", progress.operation_id);
        }
        println!("{}", progress.message);
        if !progress.recovery_directory.is_empty()
            && last.as_ref().map(|value| &value.recovery_directory)
                != Some(&progress.recovery_directory)
        {
            println!("Recovery directory: {}", progress.recovery_directory);
        }
        if matches!(progress.phase.as_known(), Some(Phase::Complete | Phase::Failed)) {
            return finish(Some(&progress));
        }
        last = Some(progress);
    }
    finish(last.as_ref())
}

fn finish(last: Option<&StorageRecoveryProgress>) -> Result<()> {
    if let Some(progress) = last {
        match progress.phase.as_known() {
            Some(Phase::Complete) => return Ok(()),
            Some(Phase::Failed) => bail!("storage operation failed: {}", progress.message),
            _ => {}
        }
    }
    bail!(
        "storage progress ended before a terminal result; the outcome is unknown and the operation may continue in the daemon; inspect ArcBox storage status before retrying"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_progress_never_reports_success() {
        assert!(finish(None).is_err());
        for phase in [
            Phase::Stopping,
            Phase::Checking,
            Phase::Restarting,
            Phase::Verifying,
        ] {
            let progress = StorageRecoveryProgress {
                phase: phase.into(),
                ..Default::default()
            };
            assert!(
                finish(Some(&progress))
                    .unwrap_err()
                    .to_string()
                    .contains("outcome is unknown")
            );
        }
        let failed = StorageRecoveryProgress {
            phase: Phase::Failed.into(),
            message: "metadata filesystem check failed".into(),
            ..Default::default()
        };
        assert!(
            finish(Some(&failed))
                .unwrap_err()
                .to_string()
                .contains(&failed.message)
        );
        let complete = StorageRecoveryProgress {
            phase: Phase::Complete.into(),
            ..Default::default()
        };
        assert!(finish(Some(&complete)).is_ok());
    }
}
