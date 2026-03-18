//! Docker socket symlink management.
//!
//! Creates or removes a symlink at `/var/run/docker.sock` pointing to the
//! user's ArcBox Docker socket. This lets Docker CLI tools find the daemon
//! without explicit `DOCKER_HOST` configuration.

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

use crate::validate;

/// Standard Docker socket path.
const DOCKER_SOCK: &str = "/var/run/docker.sock";

/// Creates a symlink at `/var/run/docker.sock` → `target`.
///
/// Idempotent: if the symlink already points to `target`, this is a no-op.
/// If it points elsewhere or is a regular file, it is replaced.
pub fn link(target: &str) -> Result<(), String> {
    validate::validate_socket_target(target)?;

    let target_path = Path::new(target);
    let link_path = Path::new(DOCKER_SOCK);

    // Check if symlink already points to the correct target.
    if let Ok(existing) = fs::read_link(link_path) {
        if existing == target_path {
            return Ok(());
        }
        // Remove stale symlink or file.
        fs::remove_file(link_path)
            .map_err(|e| format!("failed to remove existing {DOCKER_SOCK}: {e}"))?;
    }

    unix_fs::symlink(target_path, link_path)
        .map_err(|e| format!("failed to create symlink {DOCKER_SOCK} -> {target}: {e}"))
}

/// Removes the `/var/run/docker.sock` symlink.
///
/// Only removes if the path is a symlink (not a regular socket file owned
/// by another Docker daemon). Idempotent: returns Ok if already absent.
pub fn unlink() -> Result<(), String> {
    let link_path = Path::new(DOCKER_SOCK);

    match fs::symlink_metadata(link_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(link_path).map_err(|e| format!("failed to remove {DOCKER_SOCK}: {e}"))
        }
        Ok(_) => {
            // It's a real file/socket, not ours — don't touch it.
            Err(format!(
                "{DOCKER_SOCK} exists but is not a symlink (owned by another Docker daemon?)"
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to stat {DOCKER_SOCK}: {e}")),
    }
}
