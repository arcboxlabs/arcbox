//! Renders the operator's `dockerd` overrides for the guest.
//!
//! The guest agent writes `/etc/docker/daemon.json` from the keys ArcBox
//! owns (DNS, bridge pool, ulimits, image store). Everything the operator
//! adds under `[docker]` in `config.toml` reaches it through one JSON file
//! in the shared data directory, written here before every System VM boot
//! and merged by the agent at init. A missing file means "no overrides",
//! so the host removes a stale one when the section is emptied rather than
//! leaving the previous boot's mirrors in force.

use std::path::{Path, PathBuf};

use arcbox_constants::paths::guest;

use crate::config::DockerConfig;
use crate::error::{CoreError, Result};

/// Host path of the rendered override file under `data_dir`.
#[must_use]
pub fn engine_config_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(guest::CONFIG)
        .join(guest::DOCKER_ENGINE_CONFIG)
}

/// Writes the operator's `daemon.json` overrides for the guest, or removes
/// the file when there are none.
///
/// # Errors
///
/// Returns an error if the config directory cannot be created or the file
/// cannot be replaced atomically.
pub fn stage_engine_config(data_dir: &Path, docker: &DockerConfig) -> Result<()> {
    let path = engine_config_path(data_dir);
    let overrides = docker.engine_overrides();

    if overrides.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::config(format!(
                "failed to remove stale {}: {e}",
                path.display()
            ))),
        };
    }

    let dir = path.parent().expect("engine config path has a parent");
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(&overrides)
        .map_err(|e| CoreError::config(format!("failed to encode docker engine config: {e}")))?;
    // Either boot sees the previous overrides or the new ones, never a
    // truncated file; the guest treats unparsable JSON as a hard error.
    arcbox_atomic_file::write(&path, &bytes).map_err(|e| {
        CoreError::config(format!(
            "failed to write {}: {}",
            path.display(),
            e.source_io()
        ))
    })?;
    tracing::info!(
        path = %path.display(),
        keys = ?overrides.keys().collect::<Vec<_>>(),
        "staged dockerd engine overrides for the guest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_with_mirror() -> DockerConfig {
        DockerConfig {
            registry_mirrors: vec!["https://mirror.example.com".into()],
            ..DockerConfig::default()
        }
    }

    #[test]
    fn writes_overrides_and_removes_them_when_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = engine_config_path(dir.path());

        stage_engine_config(dir.path(), &docker_with_mirror()).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written["registry-mirrors"],
            serde_json::json!(["https://mirror.example.com"])
        );

        stage_engine_config(dir.path(), &DockerConfig::default()).unwrap();
        assert!(!path.exists(), "an emptied section removes the file");

        // Removing when nothing was ever written is not an error.
        stage_engine_config(dir.path(), &DockerConfig::default()).unwrap();
    }
}
