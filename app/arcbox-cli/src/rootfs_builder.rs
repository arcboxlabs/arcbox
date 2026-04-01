//! Build a Docker image from a Dockerfile and export it as a tarball.
//!
//! Runs `docker build` and `docker save` on the host via the ArcBox Docker
//! socket. The tarball is written to `~/.arcbox/bin/` so it is visible to
//! the guest VM at `/arcbox/bin/` via VirtioFS. The guest agent then handles
//! the ext4 conversion and vm-agent injection.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::process::Command;

/// Host-side output directory (VirtioFS-shared with guest as /arcbox/bin/).
fn output_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".arcbox/bin")
}

/// Build a Docker image from a Dockerfile and export it via `docker save`.
///
/// Returns `(guest_path, rootfs_type)`:
/// - Cache hit:  `("/arcbox/bin/rootfs-<hash>.ext4", "")` — ready to boot.
/// - Cache miss: `("/arcbox/bin/arcbox-save-<hash>.tar", "dockerfile")` — agent converts.
pub async fn build_and_export(dockerfile_path: &str) -> Result<(String, String)> {
    let dockerfile = Path::new(dockerfile_path);
    if !dockerfile.exists() {
        bail!("Dockerfile not found: {dockerfile_path}");
    }

    let content = tokio::fs::read(dockerfile)
        .await
        .context("failed to read Dockerfile")?;
    let hash = cache_key(&content);

    // Check if the ext4 is already cached (skip build+save+convert).
    let cached_ext4 = output_dir().join(format!("rootfs-{hash}.ext4"));
    if cached_ext4.exists() {
        eprintln!("Using cached rootfs");
        return Ok((format!("/arcbox/bin/rootfs-{hash}.ext4"), String::new()));
    }

    let tag = format!("arcbox-sandbox:{hash}");
    let tar_filename = format!("arcbox-save-{hash}.tar");
    let tar_host_path = output_dir().join(&tar_filename);
    let tar_guest_path = format!("/arcbox/bin/{tar_filename}");

    let context_dir = dockerfile
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_string_lossy()
        .to_string();

    // docker build
    eprintln!("Building Docker image...");
    let output = Command::new("docker")
        .args(["build", "-t", &tag, "-f", dockerfile_path, &context_dir])
        .output()
        .await
        .context("failed to spawn docker build")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker build failed:\n{stderr}");
    }

    // docker save
    eprintln!("Exporting image...");
    let output = Command::new("docker")
        .args(["save", &tag, "-o", &tar_host_path.to_string_lossy()])
        .output()
        .await
        .context("failed to spawn docker save")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker save failed:\n{stderr}");
    }

    eprintln!("Image exported to {}", tar_host_path.display());
    Ok((tar_guest_path, "dockerfile".to_string()))
}

fn cache_key(content: &[u8]) -> String {
    let hash = Sha256::digest(content);
    hex::encode(&hash[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_deterministic() {
        let a = cache_key(b"FROM ubuntu:22.04\nRUN apt-get update");
        let b = cache_key(b"FROM ubuntu:22.04\nRUN apt-get update");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn test_cache_key_different() {
        let a = cache_key(b"FROM ubuntu:22.04");
        let b = cache_key(b"FROM alpine:3.21");
        assert_ne!(a, b);
    }
}
