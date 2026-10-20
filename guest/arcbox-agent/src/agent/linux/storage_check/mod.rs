//! Controlled offline checks, the free space tree repair, and online write
//! verification for runtime storage.

mod btrfs_report;

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use arcbox_connect::v1::storage_check_request::Action;
use arcbox_connect::v1::storage_check_result::Finding;
use arcbox_connect::v1::storage_volume_health::{Role, State};
use arcbox_connect::v1::{StorageCheckRequest, StorageCheckResponse, StorageCheckResult};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::agent::Guest;
use crate::rpc::{ErrorResponse, RpcResponse};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_LIMIT: usize = 32 * 1024;
const TOOLS: &str = "/sbin";

pub(super) async fn handle(request: StorageCheckRequest, guest: Guest) -> RpcResponse {
    let Some(action) = request.action.as_known() else {
        return RpcResponse::Error(ErrorResponse::new(400, "unknown storage check action"));
    };
    let checked = match (guest, action) {
        (Guest::StorageRecovery, Action::OfflineCheck) => offline_checks().await,
        (Guest::StorageRecovery, Action::RepairFreeSpaceTree) => {
            repair_free_space_tree(&super::cmdline::docker_data_device(), Path::new(TOOLS))
                .await
                .map(|check| vec![check])
        }
        (Guest::SystemVm, Action::VerifyWrites) => {
            // Preserve cleanup if the host disconnects during a Docker API
            // mutation. Every Docker operation has its own deadline.
            tokio::spawn(verify_online())
                .await
                .context("write verification task failed")
                .and_then(std::convert::identity)
        }
        _ => {
            return RpcResponse::Error(ErrorResponse::new(
                403,
                "storage check action is not permitted in this guest mode",
            ));
        }
    };
    match checked {
        Ok(checks) => RpcResponse::StorageCheck(StorageCheckResponse {
            passed: !checks.is_empty() && checks.iter().all(|check| check.passed),
            checks,
            ..Default::default()
        }),
        Err(error) => RpcResponse::Error(ErrorResponse::new(
            500,
            format!("storage check failed: {error:#}"),
        )),
    }
}

async fn offline_checks() -> Result<Vec<StorageCheckResult>> {
    let data = super::cmdline::docker_data_device();
    let metadata = super::cmdline::declared_docker_metadata_device()
        .context("recovery guest has no declared metadata device")?;
    check_devices(&data, &metadata, Path::new(TOOLS)).await
}

async fn check_devices(
    data: &str,
    metadata: &str,
    tools: &Path,
) -> Result<Vec<StorageCheckResult>> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    ensure_unmounted(Path::new(data), &mountinfo)?;
    ensure_unmounted(Path::new(metadata), &mountinfo)?;
    Ok(vec![
        check_data(data, tools).await,
        check_result(
            Role::Metadata.into(),
            run_checker(&tools.join("e2fsck"), &["-f", "-n", metadata]).await,
        ),
    ])
}

/// Runs a read-only `btrfs check` and classifies a failure.
async fn check_data(device: &str, tools: &Path) -> StorageCheckResult {
    let btrfs = tools.join("btrfs");
    let run = run_tool(&btrfs, &["check", "--readonly", device]).await;
    let finding = match &run {
        Ok(run)
            if !run.status.success() && btrfs_report::only_stale_free_space_tree(&run.output) =>
        {
            Finding::StaleFreeSpaceTree
        }
        _ => Finding::FindingUnspecified,
    };
    let outcome = run.and_then(|run| {
        if run.status.success() {
            Ok(run.output)
        } else {
            Err(anyhow::anyhow!(
                "{} exited with {}: {}",
                btrfs.display(),
                run.status,
                run.output
            ))
        }
    });
    StorageCheckResult {
        finding: finding.into(),
        ..check_result(Role::Data.into(), outcome)
    }
}

/// Rebuilds the free space tree of an unmounted data device.
///
/// The host attaches a writable copy; this guest re-checks the copy rather
/// than trusting the host's classification, repairs only the one finding, and
/// passes only when a second read-only check finds nothing.
async fn repair_free_space_tree(device: &str, tools: &Path) -> Result<StorageCheckResult> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    ensure_unmounted(Path::new(device), &mountinfo)?;
    let before = check_data(device, tools).await;
    if before.finding.as_known() != Some(Finding::StaleFreeSpaceTree) {
        bail!(
            "the data volume does not have only stale free space tree entries; no repair was attempted: {}",
            before.detail
        );
    }
    let btrfs = tools.join("btrfs");
    // --force skips the interactive 10-second warning; the device is unmounted.
    let repaired = run_checker(&btrfs, &["check", "--repair", "--force", device]).await?;
    let after = check_data(device, tools).await;
    if !after.passed {
        bail!(
            "the repaired data volume still fails a read-only check: {}",
            after.detail
        );
    }
    Ok(StorageCheckResult {
        role: Role::Data.into(),
        passed: true,
        detail: format!(
            "Before repair:\n{}\nRepair:\n{repaired}\nAfter repair:\n{}",
            before.detail, after.detail
        ),
        ..Default::default()
    })
}

fn ensure_unmounted(device: &Path, mountinfo: &str) -> Result<()> {
    let metadata =
        std::fs::metadata(device).with_context(|| format!("inspect {}", device.display()))?;
    if !metadata.file_type().is_block_device() {
        bail!("{} is not a block device", device.display());
    }
    let number = metadata.rdev();
    let device_number = format!("{}:{}", libc::major(number), libc::minor(number));
    for line in mountinfo.lines() {
        let (mount, filesystem) = line.split_once(" - ").context("invalid mountinfo entry")?;
        if mount.split_whitespace().nth(2) == Some(device_number.as_str()) {
            bail!(
                "{} is mounted; offline checks are forbidden",
                device.display()
            );
        }
        let mut fields = filesystem.split_whitespace();
        let kind = fields
            .next()
            .context("mountinfo filesystem type is missing")?;
        let source = fields.next().context("mountinfo source is missing")?;
        // Btrfs reports an anonymous mount device number. Compare the actual
        // block source too so aliases and Btrfs subvolume mounts cannot evade the check.
        if kind == "btrfs" || kind == "ext4" {
            let source_metadata = std::fs::metadata(source)
                .with_context(|| format!("inspect mounted filesystem source {source}"))?;
            if source_metadata.rdev() == number {
                bail!(
                    "{} is mounted through {source}; offline checks are forbidden",
                    device.display()
                );
            }
        }
    }
    Ok(())
}

async fn run_checker(binary: &Path, args: &[&str]) -> Result<String> {
    let run = run_tool(binary, args).await?;
    if !run.status.success() {
        bail!(
            "{} exited with {}: {}",
            binary.display(),
            run.status,
            run.output
        );
    }
    Ok(run.output)
}

struct ToolRun {
    status: std::process::ExitStatus,
    /// Bounded stdout followed by bounded stderr.
    output: String,
}

async fn run_tool(binary: &Path, args: &[&str]) -> Result<ToolRun> {
    let name = binary.display();
    let mut child = tokio::process::Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("execute {name}"))?;
    let stdout = child
        .stdout
        .take()
        .context("checker stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("checker stderr is unavailable")?;
    let result = tokio::time::timeout(CHECK_TIMEOUT, async {
        tokio::join!(child.wait(), bounded_output(stdout), bounded_output(stderr))
    })
    .await;
    let (status, stdout, stderr) = match result {
        Ok(result) => result,
        Err(_) => {
            child
                .start_kill()
                .with_context(|| format!("terminate timed-out {name}"))?;
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .with_context(|| format!("reap timed-out {name}"))??;
            bail!("{name} exceeded the 30-minute check budget");
        }
    };
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout?),
        String::from_utf8_lossy(&stderr?)
    );
    Ok(ToolRun {
        status: status?,
        output,
    })
}

async fn bounded_output(mut stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut bytes = [0_u8; 4096];
    loop {
        let read = stream.read(&mut bytes).await?;
        if read == 0 {
            return Ok(output);
        }
        let keep = read.min(OUTPUT_LIMIT.saturating_sub(output.len()));
        output.extend_from_slice(&bytes[..keep]);
    }
}

async fn verify_online() -> Result<Vec<StorageCheckResult>> {
    let health = super::storage_health::snapshot().await;
    let mut checks = tokio::task::spawn_blocking(move || verify_volumes(health)).await?;
    if !checks.is_empty() && checks.iter().all(|check| check.passed) {
        checks.push(check_result(
            Role::RoleUnspecified.into(),
            super::storage_docker_probe::verify(Path::new("/bin/busybox")).await,
        ));
    }
    Ok(checks)
}

fn verify_volumes(health: arcbox_connect::v1::StorageHealth) -> Vec<StorageCheckResult> {
    let mut checks = Vec::new();
    for volume in health.volumes {
        if volume.state == State::NotConfigured {
            continue;
        }
        let outcome = if volume.state == State::MountedReadWrite {
            crate::storage_probe::verify_writes(Path::new(&volume.mount_point))
                .map(|()| {
                    "write, fsync, read-back, removal, and directory fsync succeeded".to_owned()
                })
                .map_err(anyhow::Error::from)
        } else {
            Err(anyhow::anyhow!(
                "{} is not mounted read-write: {}",
                volume.mount_point,
                volume.detail
            ))
        };
        checks.push(check_result(volume.role, outcome));
    }
    checks
}

fn check_result(role: buffa::EnumValue<Role>, outcome: Result<String>) -> StorageCheckResult {
    let (passed, detail) = match outcome {
        Ok(detail) => (true, detail),
        Err(error) => (false, format!("{error:#}")),
    };
    StorageCheckResult {
        role,
        passed,
        detail,
        ..Default::default()
    }
}

#[cfg(test)]
mod linux_integration;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn actions_cannot_cross_the_recovery_boundary() {
        for (guest, action) in [
            (Guest::SystemVm, Action::OfflineCheck),
            (Guest::SystemVm, Action::RepairFreeSpaceTree),
            (Guest::StorageRecovery, Action::VerifyWrites),
            (Guest::DistroMachine, Action::OfflineCheck),
            (Guest::DistroMachine, Action::RepairFreeSpaceTree),
            (Guest::DistroMachine, Action::VerifyWrites),
        ] {
            let response = handle(
                StorageCheckRequest {
                    action: action.into(),
                    ..Default::default()
                },
                guest,
            )
            .await;
            assert!(matches!(response, RpcResponse::Error(error) if error.code == 403));
        }
    }

    #[tokio::test]
    async fn checker_output_is_drained_but_memory_is_bounded() {
        let bytes = vec![b'x'; OUTPUT_LIMIT * 4];
        let mut input = std::io::Cursor::new(bytes);
        let output = bounded_output(&mut input).await.unwrap();
        assert_eq!(output.len(), OUTPUT_LIMIT);
        assert_eq!(input.position(), (OUTPUT_LIMIT * 4) as u64);
    }
}
