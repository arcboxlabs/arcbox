//! Docker socket symlink management.
//!
//! Creates or removes a symlink at `/var/run/docker.sock` pointing to the
//! user's ArcBox Docker socket. This lets Docker CLI tools find the daemon
//! without explicit `DOCKER_HOST` configuration.

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

use arcbox_helper::validate::SocketTarget;

/// Standard Docker socket path.
const DOCKER_SOCK: &str = arcbox_constants::paths::privileged::DOCKER_SOCKET;

/// Creates a symlink at `/var/run/docker.sock` → `target`.
///
/// Idempotent: if the symlink already points to `target`, this is a no-op.
/// If it is a symlink pointing elsewhere, it is replaced. If the path exists
/// but is not a symlink (e.g. a real socket from Docker Desktop), returns an
/// error asking the user to stop Docker Desktop first.
pub fn link(target: &SocketTarget) -> Result<(), String> {
    let target_path = Path::new(target.as_str());
    let link_path = Path::new(DOCKER_SOCK);

    match fs::symlink_metadata(link_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let existing = fs::read_link(link_path)
                .map_err(|e| format!("failed to read symlink {DOCKER_SOCK}: {e}"))?;
            if existing == target_path {
                return Ok(());
            }
            fs::remove_file(link_path)
                .map_err(|e| format!("failed to remove existing {DOCKER_SOCK}: {e}"))?;
        }
        Ok(_) => {
            return Err(format!(
                "{DOCKER_SOCK} exists but is not a symlink \
                 (is Docker Desktop running? stop it first)"
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!("failed to stat {DOCKER_SOCK}: {e}"));
        }
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
        Ok(_) => Err(format!(
            "{DOCKER_SOCK} exists but is not a symlink (owned by another Docker daemon?)"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to stat {DOCKER_SOCK}: {e}")),
    }
}
