//! Source discovery for supported migration runtimes.

use crate::error::{MigrationError, Result};
use crate::model::{SourceConfig, SourceKind};
use std::path::PathBuf;

/// Source discovery behavior shared by product adapters.
pub trait MigrationSource {
    /// Returns the source kind handled by this adapter.
    fn kind(&self) -> SourceKind;

    /// Returns the default Docker Engine socket path for this source.
    fn default_socket_path(&self) -> PathBuf;
}

/// Docker Desktop source adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct DockerDesktopSource;

impl MigrationSource for DockerDesktopSource {
    fn kind(&self) -> SourceKind {
        SourceKind::DockerDesktop
    }

    fn default_socket_path(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".docker")
            .join("run")
            .join("docker.sock")
    }
}

/// OrbStack source adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OrbStackSource;

impl MigrationSource for OrbStackSource {
    fn kind(&self) -> SourceKind {
        SourceKind::OrbStack
    }

    fn default_socket_path(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".orbstack")
            .join("run")
            .join("docker.sock")
    }
}

/// Resolves a supported migration source into a concrete socket path.
///
/// # Errors
///
/// Returns an error when the selected source socket does not exist.
pub fn resolve_source(kind: SourceKind, override_socket: Option<PathBuf>) -> Result<SourceConfig> {
    let path = if let Some(path) = override_socket {
        path
    } else {
        match kind {
            SourceKind::DockerDesktop => DockerDesktopSource.default_socket_path(),
            SourceKind::OrbStack => OrbStackSource.default_socket_path(),
        }
    };

    if !path.exists() {
        return Err(MigrationError::MissingSource {
            kind: kind.as_str(),
            path,
        });
    }

    Ok(SourceConfig {
        kind,
        socket_path: path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_desktop_default_socket_ends_with_expected_path() {
        assert!(
            DockerDesktopSource
                .default_socket_path()
                .ends_with(".docker/run/docker.sock")
        );
    }

    #[test]
    fn orbstack_default_socket_ends_with_expected_path() {
        assert!(
            OrbStackSource
                .default_socket_path()
                .ends_with(".orbstack/run/docker.sock")
        );
    }
}
