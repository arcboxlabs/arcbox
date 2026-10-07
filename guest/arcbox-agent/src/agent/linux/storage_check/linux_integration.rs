//! Privileged checks driven by tests/storage-recovery-linux.sh on disposable disks.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::process::Command;

use anyhow::ensure;
use sha2::{Digest as _, Sha256};

use super::*;

fn fixture(key: &str) -> Result<String> {
    std::env::var(format!("ARCBOX_STORAGE_TEST_{key}"))
        .with_context(|| format!("run tests/storage-recovery-linux.sh to provide {key}"))
}

fn command(binary: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(binary).args(args).output()?;
    ensure!(
        output.status.success(),
        "{binary} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn image_hash(path: &str) -> Result<Vec<u8>> {
    let mut image = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16384];
    loop {
        let bytes = image.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(hash.finalize().to_vec());
        }
        hash.update(&buffer[..bytes]);
    }
}

#[tokio::test]
#[ignore = "Requires the root-owned disposable fixtures from tests/storage-recovery-linux.sh"]
async fn clean_offline_checks_preserve_both_images() -> Result<()> {
    let data_image = fixture("DATA_IMAGE")?;
    let metadata_image = fixture("METADATA_IMAGE")?;
    let before = [image_hash(&data_image)?, image_hash(&metadata_image)?];
    let checks = check_devices(
        &fixture("DATA_DEVICE")?,
        &fixture("METADATA_DEVICE")?,
        Path::new(&fixture("TOOLS")?),
    )
    .await?;
    ensure!(
        checks.len() == 2 && checks.iter().all(|check| check.passed),
        "offline checks failed: {checks:?}"
    );
    ensure!(
        before == [image_hash(&data_image)?, image_hash(&metadata_image)?],
        "read-only checks modified an image"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the root-owned disposable fixtures from tests/storage-recovery-linux.sh"]
async fn mounted_volumes_pass_health_and_durable_write_probes() -> Result<()> {
    let data = fixture("DATA_DEVICE")?;
    let metadata = fixture("METADATA_DEVICE")?;
    let data_health =
        super::super::storage_health::observe_test_mount(&data, &fixture("DATA_MOUNT")?, "btrfs");
    let metadata_health = super::super::storage_health::observe_test_mount(
        &metadata,
        &fixture("METADATA_MOUNT")?,
        "ext4",
    );
    ensure!(
        data_health.state == State::MountedReadWrite
            && metadata_health.state == State::MountedReadWrite,
        "mount observations are not writable"
    );
    let checks = verify_volumes(arcbox_connect::v1::StorageHealth {
        volumes: vec![data_health, metadata_health],
        ..Default::default()
    });
    ensure!(
        checks.iter().all(|check| check.passed),
        "real write probes failed: {checks:?}"
    );
    ensure!(
        check_devices(&data, &metadata, Path::new(&fixture("TOOLS")?))
            .await
            .is_err(),
        "offline checks accepted mounted devices"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the disposable fixtures, static BusyBox, and a local Docker engine"]
async fn local_docker_probe_creates_runs_and_cleans_up() -> Result<()> {
    let result =
        super::super::storage_docker_probe::verify(Path::new(&fixture("BUSYBOX")?)).await?;
    eprintln!("{result}");
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the root-owned dm-error fixture from tests/storage-recovery-linux.sh"]
async fn kernel_io_failure_forces_btrfs_read_only_and_health_reports_it() -> Result<()> {
    let name = fixture("MAPPER")?;
    ensure!(
        name.starts_with("arcbox-storage-test-"),
        "refusing an unowned mapper name"
    );
    let data = fixture("DATA_DEVICE")?;
    let mount = fixture("DATA_MOUNT")?;
    let original_table = command("dmsetup", &["table", &name])?;
    let sectors = original_table
        .split_whitespace()
        .nth(1)
        .context("mapper table has no sector count")?;
    let error_table = format!("0 {sectors} error");
    let log_before = command("dmesg", &["--notime"])?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(Path::new(&mount).join("fault-probe"))?;
    file.write_all(b"healthy")?;
    file.sync_all()?;
    let result = async {
        command("dmsetup", &["suspend", "--noflush", &name])?;
        command("dmsetup", &["load", &name, "--table", &error_table])?;
        command("dmsetup", &["resume", &name])?;
        let fault_bytes = vec![0x5a; 65536].into_boxed_slice();
        let failure = file.write_all(&fault_bytes).and_then(|()| file.sync_all());
        ensure!(failure.is_err(), "dm-error did not reject the write/fsync");
        let observed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let health =
                    super::super::storage_health::observe_test_mount(&data, &mount, "btrfs");
                if health.state == State::ReadOnly {
                    return health;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("kernel did not force Btrfs read-only after an I/O failure")?;
        ensure!(
            crate::storage_probe::verify_writes(Path::new(&mount)).is_err(),
            "write probe accepted the protected volume"
        );
        let log_after = command("dmesg", &["--notime"])?;
        let new_log = log_after
            .lines()
            .skip(log_before.lines().count())
            .collect::<Vec<_>>()
            .join("\n");
        ensure!(
            new_log.contains("forced readonly"),
            "kernel did not report its forced-readonly transition: {new_log}"
        );
        eprintln!(
            "I/O failure: {failure:?}\nStorage health: {observed:?}\nKernel evidence:\n{new_log}"
        );
        Ok(())
    }
    .await;
    // Restore only this fixture's mapping so the shell can unmount and detach it.
    let restored = (|| -> Result<()> {
        command("dmsetup", &["suspend", "--noflush", &name])?;
        command(
            "dmsetup",
            &["load", &name, "--table", original_table.trim()],
        )?;
        command("dmsetup", &["resume", &name])?;
        Ok(())
    })();
    restored?;
    result
}
