//! Convert a `docker save` tarball to a bootable ext4 rootfs.
//!
//! Runs entirely inside the guest VM. The tarball is visible via VirtioFS
//! at `/arcbox/bin/`. Steps:
//!
//! 1. Validate the tarball path is under the expected directory.
//! 2. Extract the tar to a temp directory.
//! 3. Run `oci2rootfs` to produce an ext4 image.
//! 4. Inject `/sbin/vm-agent` into the ext4 via loop mount.
//! 5. Atomically rename into the cache path.
//! 6. Clean up and return the ext4 path.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use uuid::Uuid;

/// Path to the `oci2rootfs` binary (Linux aarch64, on VirtioFS).
const OCI2ROOTFS_BIN: &str = "/arcbox/bin/oci2rootfs";

/// Path to the `vm-agent` binary (trusted, inside guest).
const VM_AGENT_BIN: &str = "/arcbox/bin/vm-agent";

/// Directory for cached rootfs images.
const ROOTFS_CACHE_DIR: &str = "/arcbox/bin";

/// Expected prefix for tarball filenames.
const TAR_PREFIX: &str = "arcbox-save-";

/// Convert a `docker save` tarball to a bootable ext4 rootfs.
///
/// `tar_path` is a guest-visible path (e.g. `/arcbox/bin/arcbox-save-<hash>.tar`).
/// Returns the path to the generated ext4 image.
pub async fn convert_tar_to_rootfs(tar_path: &str) -> Result<String> {
    // Validate path is under the expected directory.
    validate_tar_path(tar_path)?;

    if !Path::new(tar_path).exists() {
        bail!("docker save tarball not found: {tar_path}");
    }

    // Extract hash from filename. Reject unexpected names.
    let hash = extract_hash(tar_path)?;
    let ext4_path = format!("{ROOTFS_CACHE_DIR}/rootfs-{hash}.ext4");

    // Use a unique request ID for temp paths to avoid concurrent collisions.
    let req_id = Uuid::new_v4().to_string();

    // Extract tar to a unique temp directory (cleaned up via scope guard).
    let oci_dir = format!("/tmp/arcbox-rootfs-{req_id}-oci");
    let _oci_guard = TempDirGuard(&oci_dir);
    extract_tar(tar_path, &oci_dir).await?;

    // Determine ext4 size from extracted content.
    let content_size = dir_size_recursive(&oci_dir).await;
    let ext4_size = (content_size * 3 / 2).max(256 * 1024 * 1024); // 1.5x, min 256MB

    // Convert to ext4 — write to temp file first, rename atomically on success.
    let ext4_tmp = format!("{ROOTFS_CACHE_DIR}/.rootfs-{req_id}.ext4.tmp");
    tracing::info!(ext4 = %ext4_path, size = ext4_size, "converting tar to ext4");
    oci_to_ext4(&oci_dir, &ext4_tmp, ext4_size).await?;

    // Inject vm-agent.
    tracing::info!("injecting vm-agent into rootfs");
    inject_vm_agent(&ext4_tmp, &req_id).await?;

    // Atomic rename into cache path.
    tokio::fs::rename(&ext4_tmp, &ext4_path)
        .await
        .context("failed to rename ext4 into cache")?;

    // Cleanup tarball.
    let _ = tokio::fs::remove_file(tar_path).await;

    tracing::info!(path = %ext4_path, "rootfs ready");
    Ok(ext4_path)
}

/// Validate that the tar path is under the expected shared directory.
fn validate_tar_path(tar_path: &str) -> Result<()> {
    let path = Path::new(tar_path);

    // Must be under /arcbox/bin/.
    if !tar_path.starts_with(ROOTFS_CACHE_DIR) {
        bail!("tarball path must be under {ROOTFS_CACHE_DIR}, got: {tar_path}");
    }

    // Reject path traversal.
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("tarball path contains path traversal: {tar_path}");
    }

    Ok(())
}

/// Extract the hash from a tarball filename like `arcbox-save-<hash>.tar`.
fn extract_hash(tar_path: &str) -> Result<String> {
    Path::new(tar_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix(TAR_PREFIX))
        .map(String::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unexpected tarball name: {tar_path} \
                 (expected pattern: {TAR_PREFIX}<hash>.tar)"
            )
        })
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

/// Recursively compute total file size of a directory tree.
async fn dir_size_recursive(path: &str) -> u64 {
    async fn walk(path: &Path) -> u64 {
        let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
            return 0;
        };

        if meta.is_file() {
            return meta.len();
        }

        if meta.is_dir() {
            let Ok(mut dir) = tokio::fs::read_dir(path).await else {
                return 0;
            };
            let mut total = 0u64;
            while let Ok(Some(entry)) = dir.next_entry().await {
                total += Box::pin(walk(&entry.path())).await;
            }
            return total;
        }

        0
    }

    let size = walk(Path::new(path)).await;
    if size > 0 { size } else { 512 * 1024 * 1024 }
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
async fn inject_vm_agent(ext4_path: &str, req_id: &str) -> Result<()> {
    if !Path::new(VM_AGENT_BIN).exists() {
        bail!("vm-agent not found at {VM_AGENT_BIN}");
    }

    let mount_dir = format!("/tmp/arcbox-inject-{req_id}");
    tokio::fs::create_dir_all(&mount_dir).await?;

    // Mount.
    let status = Command::new("/bin/busybox")
        .args(["mount", "-o", "loop", ext4_path, &mount_dir])
        .status()
        .await
        .context("failed to mount ext4 for vm-agent injection")?;
    if !status.success() {
        let _ = tokio::fs::remove_dir(&mount_dir).await;
        bail!("mount -o loop failed");
    }

    // Create /sbin and copy vm-agent.
    let sbin = format!("{mount_dir}/sbin");
    tokio::fs::create_dir_all(&sbin)
        .await
        .context("failed to create /sbin in rootfs")?;
    let dest = format!("{sbin}/vm-agent");
    let copy_result = tokio::fs::copy(VM_AGENT_BIN, &dest).await;

    // chmod 755.
    if copy_result.is_ok() {
        let _ = Command::new("/bin/busybox")
            .args(["chmod", "755", &dest])
            .status()
            .await;
    }

    // Always unmount and cleanup, even on failure.
    let _ = Command::new("/bin/busybox")
        .args(["umount", &mount_dir])
        .status()
        .await;
    let _ = tokio::fs::remove_dir(&mount_dir).await;

    copy_result.context("failed to copy vm-agent into rootfs")?;
    Ok(())
}

/// Scope guard that removes a temporary directory on drop.
struct TempDirGuard<'a>(&'a str);

impl Drop for TempDirGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_hash_valid() {
        let hash = extract_hash("/arcbox/bin/arcbox-save-abc123.tar").unwrap();
        assert_eq!(hash, "abc123");
    }

    #[test]
    fn test_extract_hash_full_sha256() {
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let path = format!("/arcbox/bin/arcbox-save-{h}.tar");
        let hash = extract_hash(&path).unwrap();
        assert_eq!(hash, h);
    }

    #[test]
    fn test_extract_hash_rejects_bad_name() {
        assert!(extract_hash("/arcbox/bin/random.tar").is_err());
        assert!(extract_hash("/arcbox/bin/arcbox-save-.tar").is_err());
    }

    #[test]
    fn test_validate_tar_path_ok() {
        assert!(validate_tar_path("/arcbox/bin/arcbox-save-abc.tar").is_ok());
    }

    #[test]
    fn test_validate_tar_path_rejects_outside() {
        assert!(validate_tar_path("/tmp/arcbox-save-abc.tar").is_err());
    }

    #[test]
    fn test_validate_tar_path_rejects_traversal() {
        assert!(validate_tar_path("/arcbox/bin/../etc/passwd").is_err());
    }
}
