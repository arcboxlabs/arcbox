//! Controlled offline checks for unmounted runtime storage.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use arcbox_connect::v1::storage_check_request::Action;
use arcbox_connect::v1::storage_volume_health::Role;
use arcbox_connect::v1::{StorageCheckRequest, StorageCheckResponse, StorageCheckResult};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::agent::Guest;
use crate::rpc::{ErrorResponse, RpcResponse};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_LIMIT: usize = 32 * 1024;

pub(super) async fn handle(request: StorageCheckRequest, guest: Guest) -> RpcResponse {
    let Some(action) = request.action.as_known() else {
        return RpcResponse::Error(ErrorResponse::new(400, "unknown storage check action"));
    };
    let checked = match (guest, action) {
        (Guest::StorageRecovery, Action::OfflineCheck) => offline_checks().await,
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
    check_devices(&data, &metadata, Path::new("/sbin")).await
}

async fn check_devices(
    data: &str,
    metadata: &str,
    tools: &Path,
) -> Result<Vec<StorageCheckResult>> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    ensure_unmounted(Path::new(data), &mountinfo)?;
    ensure_unmounted(Path::new(metadata), &mountinfo)?;
    let mut checks = Vec::new();
    for (role, binary, args) in [
        (Role::Data, "btrfs", vec!["check", "--readonly", data]),
        (Role::Metadata, "e2fsck", vec!["-f", "-n", metadata]),
    ] {
        let binary = tools.join(binary);
        let outcome = run_checker(&binary, &args).await;
        checks.push(check_result(role.into(), outcome));
    }
    Ok(checks)
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
    let detail = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout?),
        String::from_utf8_lossy(&stderr?)
    );
    let status = status?;
    if !status.success() {
        bail!("{name} exited with {status}: {detail}");
    }
    Ok(detail)
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn actions_cannot_cross_the_recovery_boundary() {
        for (guest, action) in [
            (Guest::SystemVm, Action::OfflineCheck),
            (Guest::StorageRecovery, Action::VerifyWrites),
            (Guest::DistroMachine, Action::OfflineCheck),
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
