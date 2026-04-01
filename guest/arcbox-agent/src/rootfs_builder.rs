//! Convert a `docker save` tarball to a bootable ext4 rootfs.
//!
//! Runs entirely inside the guest VM. The tarball is visible via VirtioFS
//! at `/arcbox/bin/`. Steps:
//!
//! 1. Extract the tar to a temp directory.
//! 2. Run `oci2rootfs` to produce an ext4 image.
//! 3. Inject `/sbin/vm-agent` into the ext4 via loop mount.
//! 4. Clean up and return the ext4 path.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Path to the `oci2rootfs` binary (Linux aarch64, on VirtioFS).
const OCI2ROOTFS_BIN: &str = "/arcbox/bin/oci2rootfs";

/// Path to the `vm-agent` binary (trusted, inside guest).
const VM_AGENT_BIN: &str = "/arcbox/bin/vm-agent";

/// Directory for cached rootfs images.
const ROOTFS_CACHE_DIR: &str = "/arcbox/bin";

/// Convert a `docker save` tarball to a bootable ext4 rootfs.
///
/// `tar_path` is a guest-visible path (e.g. `/arcbox/bin/arcbox-save-<hash>.tar`).
/// Returns the path to the generated ext4 image.
pub async fn convert_tar_to_rootfs(tar_path: &str) -> Result<String> {
    if !Path::new(tar_path).exists() {
        bail!("docker save tarball not found: {tar_path}");
    }

    // Derive the ext4 output path from the tar filename.
    // CLI names tars as `arcbox-save-<hash>.tar`, so the ext4 becomes `rootfs-<hash>.ext4`.
    let hash = Path::new(tar_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("arcbox-save-"))
        .unwrap_or("unknown");
    let ext4_path = format!("{ROOTFS_CACHE_DIR}/rootfs-{hash}.ext4");

    // Extract tar to temp directory.
    let oci_dir = format!("/tmp/arcbox-rootfs-{hash}-oci");
    extract_tar(tar_path, &oci_dir).await?;

    // Determine ext4 size from extracted content.
    let content_size = dir_size(&oci_dir).await;
    let ext4_size = (content_size * 3 / 2).max(256 * 1024 * 1024); // 1.5x, min 256MB

    // Convert to ext4.
    tracing::info!(ext4 = %ext4_path, size = ext4_size, "converting tar to ext4");
    oci_to_ext4(&oci_dir, &ext4_path, ext4_size).await?;

    // Inject vm-agent.
    tracing::info!("injecting vm-agent into rootfs");
    inject_vm_agent(&ext4_path).await?;

    // Cleanup.
    let _ = tokio::fs::remove_dir_all(&oci_dir).await;
    let _ = tokio::fs::remove_file(tar_path).await;

    tracing::info!(path = %ext4_path, "rootfs ready");
    Ok(ext4_path)
}

async fn extract_tar(tar_path: &str, dest: &str) -> Result<()> {
    tokio::fs::create_dir_all(dest).await?;
    let output = Command::new("/bin/busybox")
        .args(["tar", "-xf", tar_path, "-C", dest])
        .output()
        .await
        .context("failed to extract tar")?;

    if !output.status.success() {
        bail!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Estimate total size of a directory tree in bytes.
async fn dir_size(path: &str) -> u64 {
    let output = Command::new("/bin/busybox")
        .args(["du", "-sb", path])
        .output()
        .await
        .ok();
    output
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(512 * 1024 * 1024)
}

async fn oci_to_ext4(oci_dir: &str, ext4_path: &str, size: u64) -> Result<()> {
    if !Path::new(OCI2ROOTFS_BIN).exists() {
        bail!("oci2rootfs not found at {OCI2ROOTFS_BIN}");
    }

    let output = Command::new(OCI2ROOTFS_BIN)
        .args([oci_dir, "--output", ext4_path, "--size", &size.to_string()])
        .output()
        .await
        .context("failed to spawn oci2rootfs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("oci2rootfs failed: {stderr}");
    }
    Ok(())
}

/// Inject vm-agent into an ext4 image via loop mount.
async fn inject_vm_agent(ext4_path: &str) -> Result<()> {
    if !Path::new(VM_AGENT_BIN).exists() {
        bail!("vm-agent not found at {VM_AGENT_BIN}");
    }

    let mount_dir = format!("/tmp/arcbox-inject-{}", std::process::id());
    tokio::fs::create_dir_all(&mount_dir).await?;

    // Mount.
    let status = Command::new("/bin/busybox")
        .args(["mount", "-o", "loop", ext4_path, &mount_dir])
        .status()
        .await
        .context("failed to mount ext4 for vm-agent injection")?;
    if !status.success() {
        bail!("mount -o loop failed");
    }

    // Create /sbin and copy vm-agent.
    let sbin = format!("{mount_dir}/sbin");
    tokio::fs::create_dir_all(&sbin).await.ok();
    let dest = format!("{sbin}/vm-agent");
    let copy_result = tokio::fs::copy(VM_AGENT_BIN, &dest).await;

    // chmod 755.
    if copy_result.is_ok() {
        let _ = Command::new("/bin/busybox")
            .args(["chmod", "755", &dest])
            .status()
            .await;
    }

    // Always unmount, even if copy failed.
    let _ = Command::new("/bin/busybox")
        .args(["umount", &mount_dir])
        .status()
        .await;
    let _ = tokio::fs::remove_dir(&mount_dir).await;

    copy_result.context("failed to copy vm-agent")?;
    Ok(())
}
