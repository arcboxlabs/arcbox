//! Build a Docker image from a Dockerfile and export it as a tarball.
//!
//! Runs `docker build` and `docker save` on the host via the ArcBox Docker
//! context. The tarball is written to `~/.arcbox/bin/` so it is visible to
//! the guest VM at `/arcbox/bin/` via VirtioFS. The guest agent then handles
//! the ext4 conversion and vm-agent injection.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::process::Command;

/// Docker context name used for ArcBox Docker API.
const DOCKER_CONTEXT: &str = "arcbox";

/// Host-side output directory (VirtioFS-shared with guest as /arcbox/bin/).
fn output_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".arcbox/bin");
    Ok(dir)
}

/// Check if a file has a valid ext4 superblock magic.
pub fn has_ext4_magic(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.seek(SeekFrom::Start(0x438)).is_ok()
        && file.read_exact(&mut magic).is_ok()
        && magic == [0x53, 0xEF]
}

/// Check if a file looks like a Dockerfile (first non-comment line starts with FROM).
pub fn looks_like_dockerfile(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content
        .lines()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .is_some_and(|line| line.trim().to_ascii_uppercase().starts_with("FROM "))
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
    if !looks_like_dockerfile(dockerfile) {
        bail!(
            "{dockerfile_path} does not appear to be a Dockerfile \
             (expected first non-comment line to start with FROM)"
        );
    }

    let content = tokio::fs::read(dockerfile)
        .await
        .context("failed to read Dockerfile")?;
    let hash = cache_key(&content);

    let out = output_dir()?;
    tokio::fs::create_dir_all(&out).await?;

    // Check if the ext4 is already cached (skip build+save+convert).
    // Validate the cached file has ext4 magic to avoid using corrupt artifacts.
    let cached_ext4 = out.join(format!("rootfs-{hash}.ext4"));
    if cached_ext4.exists() && has_ext4_magic(&cached_ext4) {
        eprintln!("Using cached rootfs");
        return Ok((format!("/arcbox/bin/rootfs-{hash}.ext4"), String::new()));
    }

    let tag = format!("arcbox-sandbox:{hash}");
    let tar_filename = format!("arcbox-save-{hash}.tar");
    let tar_host_path = out.join(&tar_filename);
    let tar_guest_path = format!("/arcbox/bin/{tar_filename}");

    let context_dir = dockerfile
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_string_lossy()
        .to_string();

    // docker build
    eprintln!("Building Docker image...");
    let output = Command::new("docker")
        .args([
            "--context",
            DOCKER_CONTEXT,
            "build",
            "-t",
            &tag,
            "-f",
            dockerfile_path,
            &context_dir,
        ])
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
        .args([
            "--context",
            DOCKER_CONTEXT,
            "save",
            &tag,
            "-o",
            &tar_host_path.to_string_lossy(),
        ])
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

/// Full SHA-256 hex digest of content.
fn cache_key(content: &[u8]) -> String {
    let hash = Sha256::digest(content);
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_deterministic() {
        let a = cache_key(b"FROM ubuntu:22.04\nRUN apt-get update");
        let b = cache_key(b"FROM ubuntu:22.04\nRUN apt-get update");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // full SHA-256
    }

    #[test]
    fn test_cache_key_different() {
        let a = cache_key(b"FROM ubuntu:22.04");
        let b = cache_key(b"FROM alpine:3.21");
        assert_ne!(a, b);
    }

    #[test]
    fn test_looks_like_dockerfile() {
        let dir = tempfile::TempDir::new().unwrap();
        let df = dir.path().join("Dockerfile");

        std::fs::write(&df, "FROM ubuntu:22.04\nRUN echo hi").unwrap();
        assert!(looks_like_dockerfile(&df));

        std::fs::write(&df, "# comment\nFROM alpine").unwrap();
        assert!(looks_like_dockerfile(&df));

        std::fs::write(&df, "not a dockerfile").unwrap();
        assert!(!looks_like_dockerfile(&df));

        std::fs::write(&df, "").unwrap();
        assert!(!looks_like_dockerfile(&df));
    }

    #[test]
    fn test_has_ext4_magic_nonexistent() {
        assert!(!has_ext4_magic(Path::new("/nonexistent")));
    }
}
