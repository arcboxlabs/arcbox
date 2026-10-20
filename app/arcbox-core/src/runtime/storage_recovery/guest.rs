//! Runs filesystem tools in a disposable guest: read-only checks of the
//! preserved snapshots, and the free space tree repair of a writable copy.

use crate::{
    Runtime,
    error::{CoreError, Result},
};
use arcbox_connect::v1::{
    StorageCheckRequest, StorageCheckResponse, StorageRecoveryProgress,
    storage_check_request::Action, storage_check_result::Finding, storage_volume_health::Role,
};
use arcbox_engine::{
    agent_client::AgentClient,
    machine::{DEFAULT_MACHINE_NAME, MachineConfig, MachineInfo, MachineManager},
    vm::BlockDeviceConfig,
};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// What the offline check found.
pub(super) enum Verdict {
    Passed,
    /// The data volume's only problem is stale free space tree entries; the
    /// metadata volume passed.
    StaleFreeSpaceTree,
    Failed(String),
}

pub(super) struct OfflineOutcome<T> {
    pub(super) checked: Result<T>,
    pub(super) cleaned: Result<()>,
}

pub(super) async fn offline_check(
    runtime: &Runtime,
    original: &MachineInfo,
    devices: Vec<BlockDeviceConfig>,
    progress: &StorageRecoveryProgress,
) -> Result<OfflineOutcome<Verdict>> {
    run_disposable(
        runtime,
        original,
        devices,
        &format!("storage-check-{}", progress.operation_id),
        Action::OfflineCheck,
        |response| {
            record_check(progress, "offline-check.txt", response)?;
            Ok(verdict(response))
        },
    )
    .await
}

/// Repairs the free space tree of the writable data copy in `devices`.
pub(super) async fn repair_free_space_tree(
    runtime: &Runtime,
    original: &MachineInfo,
    devices: Vec<BlockDeviceConfig>,
    progress: &StorageRecoveryProgress,
) -> Result<OfflineOutcome<()>> {
    run_disposable(
        runtime,
        original,
        devices,
        &format!("storage-repair-{}", progress.operation_id),
        Action::RepairFreeSpaceTree,
        |response| {
            record_check(progress, "free-space-tree-repair.txt", response)?;
            require_pass(response)
        },
    )
    .await
}

async fn run_disposable<T>(
    runtime: &Runtime,
    original: &MachineInfo,
    devices: Vec<BlockDeviceConfig>,
    name: &str,
    action: Action,
    interpret: impl FnOnce(&StorageCheckResponse) -> Result<T>,
) -> Result<OfflineOutcome<T>> {
    let cancelled = &runtime.storage_recovery.owner.cancelled;
    runtime.storage_recovery.owner.check_open()?;
    let manager = &runtime.machine_manager;
    let mut tokens: Vec<&str> = original
        .cmdline
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|token| {
            !token.starts_with("init=") && !token.starts_with("arcbox.storage_recovery=")
        })
        .collect();
    tokens.extend([
        "init=/sbin/arcbox-storage-recovery",
        "arcbox.storage_recovery=1",
    ]);
    manager
        .create(MachineConfig {
            name: name.to_owned(),
            cpus: original.cpus.min(4),
            memory_mb: original.memory_mb,
            kernel: original.kernel.clone(),
            cmdline: Some(tokens.join(" ")),
            block_devices: devices,
            backend: original.backend,
            ..Default::default()
        })
        .await?;
    let checked = async {
        runtime.storage_recovery.owner.check_open()?;
        manager.start(name).await?;
        let agent = ready_agent(Arc::clone(manager), name.to_owned(), cancelled).await?;
        let response = agent
            .storage_check_with_cancel(
                StorageCheckRequest {
                    action: action.into(),
                    ..Default::default()
                },
                cancelled,
            )
            .await?;
        interpret(&response)
    }
    .await;
    // Removal tears down the temporary VM before the original can restart.
    let manager = Arc::clone(manager);
    let name = name.to_owned();
    let cleaned = tokio::task::spawn_blocking(move || manager.remove(&name, true))
        .await
        .map_err(|error| CoreError::Machine(format!("remove recovery VM task: {error}")))
        .and_then(|result| result.map_err(CoreError::from));
    Ok(OfflineOutcome { checked, cleaned })
}

fn verdict(response: &StorageCheckResponse) -> Verdict {
    if response.passed {
        return Verdict::Passed;
    }
    let failed: Vec<_> = response
        .checks
        .iter()
        .filter(|check| !check.passed)
        .collect();
    match failed.as_slice() {
        [check]
            if check.role.as_known() == Some(Role::Data)
                && check.finding.as_known() == Some(Finding::StaleFreeSpaceTree)
                && response.checks.len() == 2 =>
        {
            Verdict::StaleFreeSpaceTree
        }
        _ => Verdict::Failed(failure_detail(response)),
    }
}

pub(super) async fn wait_runtime(runtime: &Runtime) -> Result<()> {
    let manager = Arc::clone(&runtime.machine_manager);
    let agent = tokio::task::spawn_blocking(move || manager.connect_agent(DEFAULT_MACHINE_NAME))
        .await
        .map_err(|error| CoreError::Machine(format!("connect runtime readiness: {error}")))??;
    let budget = Duration::from_secs(150);
    agent
        .watch_readiness_with_cancel(
            true,
            budget,
            "storage-recovery",
            &runtime.storage_recovery.owner.cancelled,
        )
        .await?;
    Ok(())
}

pub(super) async fn verify_writes(
    runtime: &Runtime,
    progress: &StorageRecoveryProgress,
) -> Result<()> {
    let cancelled = &runtime.storage_recovery.owner.cancelled;
    let manager = Arc::clone(&runtime.machine_manager);
    let agent = tokio::task::spawn_blocking(move || manager.connect_agent(DEFAULT_MACHINE_NAME))
        .await
        .map_err(|error| CoreError::Machine(format!("connect storage verifier: {error}")))??;
    let response = agent
        .storage_check_with_cancel(
            StorageCheckRequest {
                action: Action::VerifyWrites.into(),
                ..Default::default()
            },
            cancelled,
        )
        .await.map_err(|error| {
            if cancelled.is_cancelled() {
                CoreError::Machine(format!("{error}; write verification and guest Docker probe cleanup are unconfirmed; storage remains protected"))
            } else {
                CoreError::from(error)
            }
        })?;
    record_check(progress, "write-verification.txt", &response)?;
    require_pass(&response)
}

async fn ready_agent(
    manager: Arc<MachineManager>,
    name: String,
    cancelled: &tokio_util::sync::CancellationToken,
) -> Result<AgentClient> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if cancelled.is_cancelled() {
            return Err(CoreError::invalid_state(
                "recovery agent handshake cancelled",
            ));
        }
        let probe_manager = Arc::clone(&manager);
        let probe_name = name.clone();
        let probe = tokio::task::spawn_blocking(move || probe_manager.connect_agent(&probe_name))
            .await
            .map_err(|error| CoreError::Machine(format!("recovery readiness task: {error}")))?;
        let result = match probe {
            Ok(agent) => agent.ping_with_cancel(cancelled).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(ping) => {
                AgentClient::check_agent_protocol(&ping)?;
                return tokio::task::spawn_blocking(move || manager.connect_agent(&name))
                    .await
                    .map_err(|error| {
                        CoreError::Machine(format!("connect recovery checker: {error}"))
                    })?
                    .map_err(CoreError::from);
            }
            Err(error) if cancelled.is_cancelled() || Instant::now() >= deadline => {
                return Err(error.into());
            }
            Err(error) => tracing::debug!(%error, "recovery agent has not answered yet"),
        }
        tokio::select! {
            () = cancelled.cancelled() => {},
            () = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}

fn record_check(
    progress: &StorageRecoveryProgress,
    filename: &str,
    response: &StorageCheckResponse,
) -> Result<()> {
    let report = response
        .checks
        .iter()
        .map(|result| {
            format!(
                "{:?}: passed={}\n{}\n",
                result.role, result.passed, result.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    arcbox_atomic_file::write(
        &Path::new(&progress.recovery_directory).join(filename),
        report.as_bytes(),
    )?;
    Ok(())
}

fn require_pass(response: &StorageCheckResponse) -> Result<()> {
    if !response.passed {
        return Err(CoreError::Machine(failure_detail(response)));
    }
    Ok(())
}

fn failure_detail(response: &StorageCheckResponse) -> String {
    response
        .checks
        .iter()
        .filter(|result| !result.passed)
        .map(|result| result.detail.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_connect::v1::StorageCheckResult;

    fn result(role: Role, passed: bool, finding: Finding) -> StorageCheckResult {
        StorageCheckResult {
            role: role.into(),
            passed,
            detail: format!("{role:?}"),
            finding: finding.into(),
            ..Default::default()
        }
    }

    fn response(checks: Vec<StorageCheckResult>) -> StorageCheckResponse {
        StorageCheckResponse {
            passed: checks.iter().all(|check| check.passed),
            checks,
            ..Default::default()
        }
    }

    #[test]
    fn only_a_stale_data_tree_beside_a_passing_metadata_volume_is_repairable() {
        let stale = || result(Role::Data, false, Finding::StaleFreeSpaceTree);
        let clean = |role| result(role, true, Finding::FindingUnspecified);
        assert!(matches!(
            verdict(&response(vec![stale(), clean(Role::Metadata)])),
            Verdict::StaleFreeSpaceTree
        ));
        assert!(matches!(
            verdict(&response(vec![clean(Role::Data), clean(Role::Metadata)])),
            Verdict::Passed
        ));
        for checks in [
            vec![
                stale(),
                result(Role::Metadata, false, Finding::FindingUnspecified),
            ],
            vec![
                result(Role::Data, false, Finding::FindingUnspecified),
                clean(Role::Metadata),
            ],
            vec![
                clean(Role::Data),
                result(Role::Metadata, false, Finding::StaleFreeSpaceTree),
            ],
            vec![stale()],
        ] {
            assert!(matches!(verdict(&response(checks)), Verdict::Failed(_)));
        }
    }
}
