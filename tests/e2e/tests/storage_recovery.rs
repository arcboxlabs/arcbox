//! Exercises recovery, interrupted-operation replay, and storage faults on disposable disks.

use anyhow::{Context, Result, bail, ensure};
use arcbox_e2e::{
    boot_assets::{resolve_boot_version, stage_dev_boot_assets},
    daemon::{DaemonConfig, DaemonHandle, connect_unix},
};
use arcbox_grpc::v1::system_service_client::SystemServiceClient;
use arcbox_protocol::v1::{
    Empty, RecoverStorageRequest, recover_storage_request::Action, storage_recovery_progress::Phase,
};
use std::{
    fs::OpenOptions,
    os::unix::fs::FileExt,
    path::Path,
    time::{Duration, Instant},
};
use tonic::transport::Channel;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots an isolated System VM and a recovery VM; requires matching development assets"]
async fn paired_storage_check_restart_and_metadata_faults() -> Result<()> {
    let root = arcbox_e2e::repo_root();
    let version = resolve_boot_version(&root)?;
    let directory = tempfile::Builder::new()
        .prefix("arcbox-storage-recovery-")
        .tempdir()?;
    stage_dev_boot_assets(&root, directory.path(), &version)?;
    let mut daemon = None;
    let started = Instant::now();
    let result = async {
        scenario(&root, directory.path(), &version, &mut daemon).await?;
        if let Some(daemon) = daemon.take() {
            let status = tokio::task::block_in_place(|| daemon.shutdown())?;
            ensure!(status.success(), "test daemon teardown failed: {status}");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    std::fs::write(
        directory.path().join("metrics.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "scenario": "storage_recovery", "passed": result.is_ok(), "elapsed_seconds": started.elapsed().as_secs_f64(),
            "backend": std::env::var("ARCBOX_VM_BACKEND").unwrap_or_else(|_| "vz".into()),
        }))?,
    )?;
    if let Err(error) = result {
        if let Some(daemon) = daemon.as_ref() {
            match tokio::task::block_in_place(|| daemon.dump_virtio_debug()) {
                Ok(path) => eprintln!("Recovery diagnostics: {}", path.display()),
                Err(error) => eprintln!("Could not capture recovery diagnostics: {error}"),
            }
        }
        drop(daemon);
        let path = directory.keep();
        bail!(
            "{error:#}; preserved isolated data directory: {}",
            path.display()
        );
    }
    Ok(())
}

async fn scenario(
    root: &Path,
    directory: &Path,
    version: &str,
    daemon: &mut Option<DaemonHandle>,
) -> Result<()> {
    start(root, directory, version, daemon).await?;
    let mut client = client(directory).await?;
    let checked = run(&mut client, Action::CheckOnly).await?;
    ensure!(
        checked.phase() == Phase::Complete,
        "offline check failed: {}",
        checked.message
    );
    ensure!(
        directory.join("storage-recovery/hold").is_file(),
        "check-only must retain its boot hold"
    );
    ensure!(
        checked.storage_protected,
        "check-only incorrectly cleared typed protection"
    );
    ensure!(
        Path::new(&checked.recovery_directory)
            .join("offline-check.txt")
            .is_file()
    );
    let original = daemon.take().context("initial daemon")?;
    let status = tokio::task::block_in_place(|| original.shutdown())?;
    ensure!(status.success(), "initial daemon teardown failed: {status}");

    start(root, directory, version, daemon).await?;
    client = self::client(directory).await?;
    let status = client.get_setup_status(Empty {}).await?.into_inner();
    ensure!(
        !status.vm_running,
        "daemon restart bypassed the durable storage hold"
    );
    let replay = status
        .storage_recovery
        .context("replayed recovery status")?;
    ensure!(replay.operation_id == checked.operation_id);
    ensure!(replay.storage_protected, "replay lost storage protection");
    let recovered = run(&mut client, Action::Recover).await?;
    ensure!(
        recovered.phase() == Phase::Complete,
        "write verification failed: {}",
        recovered.message
    );
    ensure!(!directory.join("storage-recovery/hold").exists());
    ensure!(
        !recovered.storage_protected,
        "verified recovery retained typed protection"
    );
    let report = std::fs::read_to_string(
        Path::new(&recovered.recovery_directory).join("write-verification.txt"),
    )?;
    ensure!(
        report.contains("Docker"),
        "Docker lifecycle verification was not recorded: {report}"
    );

    let original = daemon.take().context("recovered daemon")?;
    let status = tokio::task::block_in_place(|| original.shutdown())?;
    ensure!(
        status.success(),
        "recovered daemon teardown failed: {status}"
    );
    let metadata = directory.join("data/docker-meta.img");
    mount_failure(root, directory, version, daemon).await?;
    let saved = directory.join("data/docker-meta.removed-by-test");
    std::fs::rename(&metadata, &saved)?;
    start(root, directory, version, daemon).await?;
    client = self::client(directory).await?;
    let status = client.get_setup_status(Empty {}).await?.into_inner();
    ensure!(!status.vm_running, "invalid storage was booted");
    let protection = status.storage_recovery.context("startup storage failure")?;
    ensure!(protection.phase() == Phase::Failed);
    ensure!(protection.storage_protected);
    let failed = run(&mut client, Action::Recover).await?;
    ensure!(
        failed.phase() == Phase::Failed,
        "missing metadata was accepted"
    );
    ensure!(failed.storage_protected);
    ensure!(
        !metadata.exists(),
        "missing metadata was silently recreated"
    );
    ensure!(saved.is_file(), "removed member was changed");
    ensure!(directory.join("storage-recovery/hold").is_file());
    Ok(())
}

async fn mount_failure(
    root: &Path,
    directory: &Path,
    version: &str,
    daemon: &mut Option<DaemonHandle>,
) -> Result<()> {
    let metadata = directory.join("data/docker-meta.img");
    // The geometry fault preserves the identity and signature checked by the host.
    let original_blocks = replace_ext4_block_count(&metadata, u32::MAX.to_le_bytes())?;
    arcbox_storage::verify_pair(&directory.join("data/docker.img"), &metadata)?;
    start(root, directory, version, daemon).await?;
    let mut client = self::client(directory).await?;
    let status = client.get_setup_status(Empty {}).await?.into_inner();
    ensure!(
        !status.vm_running,
        "guest mount failure left the System VM running"
    );
    let protection = status
        .storage_recovery
        .context("guest mount failure protection")?;
    ensure!(protection.phase() == Phase::Failed);
    ensure!(protection.storage_protected);
    let diagnostic = std::fs::read_to_string(
        Path::new(&protection.recovery_directory).join("startup-runtime-status.txt"),
    )?;
    ensure!(
        diagnostic.contains("metadata volume setup failed"),
        "original mount error was lost: {diagnostic}"
    );
    ensure!(
        diagnostic.contains("UNAVAILABLE"),
        "fresh storage fault was not recorded: {diagnostic}"
    );

    let failed = run(&mut client, Action::Recover).await?;
    ensure!(
        failed.phase() == Phase::Failed,
        "invalid ext4 geometry passed recovery"
    );
    ensure!(failed.storage_protected);
    ensure!(directory.join("storage-recovery/hold").is_file());
    let report =
        std::fs::read_to_string(Path::new(&failed.recovery_directory).join("offline-check.txt"))?;
    ensure!(
        report.contains("METADATA: passed=false"),
        "offline check did not record the filesystem failure: {report}"
    );
    let original = daemon.take().context("protected daemon")?;
    let status = tokio::task::block_in_place(|| original.shutdown())?;
    ensure!(
        status.success(),
        "protected daemon teardown failed: {status}"
    );

    // Restore only the injected bytes after the disposable VM has stopped.
    ensure!(
        replace_ext4_block_count(&metadata, original_blocks)? == u32::MAX.to_le_bytes(),
        "recovery modified the original fault"
    );
    start(root, directory, version, daemon).await?;
    client = self::client(directory).await?;
    let status = client.get_setup_status(Empty {}).await?.into_inner();
    ensure!(
        !status.vm_running,
        "restoring the fixture bypassed the durable hold"
    );
    let recovered = run(&mut client, Action::Recover).await?;
    ensure!(
        recovered.phase() == Phase::Complete,
        "restored fixture could not recover: {}",
        recovered.message
    );
    ensure!(!recovered.storage_protected);
    ensure!(!directory.join("storage-recovery/hold").exists());
    let original = daemon.take().context("restored daemon")?;
    let status = tokio::task::block_in_place(|| original.shutdown())?;
    ensure!(
        status.success(),
        "restored daemon teardown failed: {status}"
    );
    Ok(())
}

fn replace_ext4_block_count(metadata: &Path, blocks: [u8; 4]) -> Result<[u8; 4]> {
    let file = OpenOptions::new().read(true).write(true).open(metadata)?;
    let mut previous = [0; 4];
    file.read_exact_at(&mut previous, 1028)?;
    file.write_all_at(&blocks, 1028)?;
    file.sync_all()?;
    Ok(previous)
}

async fn start(
    root: &Path,
    directory: &Path,
    version: &str,
    daemon: &mut Option<DaemonHandle>,
) -> Result<()> {
    *daemon = Some(DaemonHandle::spawn(DaemonConfig {
        binary: root.join("target/release/arcbox-daemon"),
        data_dir: directory.to_owned(),
        args: vec!["--profile".into(), "development".into()],
        env: vec![
            ("ARCBOX_BOOT_ASSET_VERSION".into(), version.into()),
            (
                "ARCBOX_VM_BACKEND".into(),
                std::env::var("ARCBOX_VM_BACKEND").unwrap_or_else(|_| "vz".into()),
            ),
        ],
    })?);
    daemon
        .as_mut()
        .context("spawned test daemon")?
        .wait_ready(Duration::from_secs(240))
        .await?;
    Ok(())
}

async fn client(directory: &Path) -> Result<SystemServiceClient<Channel>> {
    Ok(SystemServiceClient::new(
        connect_unix(&directory.join("run/arcbox.sock")).await?,
    ))
}

async fn run(
    client: &mut SystemServiceClient<Channel>,
    action: Action,
) -> Result<arcbox_protocol::v1::StorageRecoveryProgress> {
    let mut stream = client
        .recover_storage(RecoverStorageRequest {
            action: action.into(),
        })
        .await?
        .into_inner();
    let mut last = None;
    loop {
        let progress = tokio::time::timeout(Duration::from_secs(3700), stream.message()).await??;
        let Some(progress) = progress else { break };
        eprintln!(
            "Recovery {} {:?}: {}",
            progress.operation_id,
            progress.phase(),
            progress.message
        );
        last = Some(progress);
    }
    let last = last.context("recovery stream returned no progress")?;
    ensure!(
        matches!(last.phase(), Phase::Complete | Phase::Failed),
        "recovery stream ended without a terminal outcome"
    );
    Ok(last)
}
