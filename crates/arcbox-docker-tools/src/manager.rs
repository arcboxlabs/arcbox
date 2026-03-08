//! Docker tool manager — download, extract, install, and validate Docker CLI
//! binaries from the versions pinned in `assets.lock`.

use crate::lockfile::ToolEntry;
use crate::registry::{self, ArtifactFormat};
use arcbox_asset::download::{download_and_verify, sha256_file};
use arcbox_asset::{PreparePhase, PrepareProgress, ProgressCallback};
use std::path::{Path, PathBuf};
use tracing::info;

/// Manages Docker CLI tool installation.
pub struct DockerToolManager {
    /// Architecture string (e.g. "arm64", "x86_64").
    arch: String,
    /// Directory for downloaded artifacts (e.g. `~/.arcbox/runtime/bin/`).
    install_dir: PathBuf,
    /// Tool entries parsed from `assets.lock`.
    tools: Vec<ToolEntry>,
    /// Optional directory containing pre-built binaries from an app bundle
    /// (e.g. `Contents/MacOS/xbin/`).
    bundle_dir: Option<PathBuf>,
}

impl DockerToolManager {
    /// Create a new manager from parsed tool entries.
    #[must_use]
    pub fn new(tools: Vec<ToolEntry>, arch: impl Into<String>, install_dir: PathBuf) -> Self {
        Self {
            arch: arch.into(),
            install_dir,
            tools,
            bundle_dir: None,
        }
    }

    /// Set an app-bundle directory containing pre-built binaries.
    ///
    /// When set, `install_one` will try to copy from this directory before
    /// falling back to a CDN download.
    #[must_use]
    pub fn with_bundle_dir(mut self, dir: PathBuf) -> Self {
        self.bundle_dir = Some(dir);
        self
    }

    /// Install all configured Docker tools.
    ///
    /// For each tool: check cache → try bundle → download → verify → extract → chmod.
    pub async fn install_all(
        &self,
        progress: Option<&ProgressCallback>,
    ) -> Result<(), DockerToolError> {
        tokio::fs::create_dir_all(&self.install_dir)
            .await
            .map_err(DockerToolError::Io)?;

        let total = self.tools.len();
        for (idx, tool) in self.tools.iter().enumerate() {
            self.install_one(tool, idx + 1, total, progress).await?;
        }

        Ok(())
    }

    /// Install a single tool.
    async fn install_one(
        &self,
        tool: &ToolEntry,
        current: usize,
        total: usize,
        progress: Option<&ProgressCallback>,
    ) -> Result<(), DockerToolError> {
        let expected_sha =
            tool.sha256_for_arch(&self.arch)
                .ok_or_else(|| DockerToolError::UnsupportedArch {
                    tool: tool.name.clone(),
                    arch: self.arch.clone(),
                })?;

        let dest = self.install_dir.join(&tool.name);
        let format = registry::artifact_format(&tool.name);

        let pg = |phase: PreparePhase| {
            if let Some(cb) = progress {
                cb(PrepareProgress {
                    name: tool.name.clone(),
                    current,
                    total,
                    phase,
                });
            }
        };

        pg(PreparePhase::Checking);

        // Check cache: if binary exists and version matches, skip.
        if dest.exists() && self.is_cached(&tool.name, expected_sha, format).await {
            pg(PreparePhase::Cached);
            info!(tool = %tool.name, "already installed, checksum matches");
            return Ok(());
        }

        // Try to install from app bundle before downloading from CDN.
        if self
            .try_install_from_bundle(tool, &dest, expected_sha, format)
            .await?
        {
            mark_executable(&dest).await?;
            write_sidecar(&self.install_dir, &tool.name, expected_sha).await?;
            pg(PreparePhase::Ready);
            info!(tool = %tool.name, version = %tool.version, "installed from bundle");
            return Ok(());
        }

        let url = registry::download_url(&tool.name, &tool.version, &self.arch);

        match format {
            ArtifactFormat::Binary => {
                // Direct binary download — verified by arcbox-asset.
                download_and_verify(&url, &dest, expected_sha, &tool.name, |dl, tot| {
                    pg(PreparePhase::Downloading {
                        downloaded: dl,
                        total: tot,
                    });
                })
                .await
                .map_err(DockerToolError::Asset)?;
            }
            ArtifactFormat::Tgz => {
                // Download tgz to temp, verify checksum, then extract the binary.
                let tgz_path = self.install_dir.join(format!("{}.tgz", tool.name));
                download_and_verify(&url, &tgz_path, expected_sha, &tool.name, |dl, tot| {
                    pg(PreparePhase::Downloading {
                        downloaded: dl,
                        total: tot,
                    });
                })
                .await
                .map_err(DockerToolError::Asset)?;

                pg(PreparePhase::Verifying);
                extract_from_tgz(&tgz_path, registry::tgz_inner_path(&tool.name), &dest)?;
                let _ = tokio::fs::remove_file(&tgz_path).await;
            }
        }

        mark_executable(&dest).await?;
        write_sidecar(&self.install_dir, &tool.name, expected_sha).await?;

        pg(PreparePhase::Ready);
        info!(tool = %tool.name, version = %tool.version, "installed");
        Ok(())
    }

    /// Check whether the installed binary is up-to-date.
    ///
    /// For `Binary` artifacts the SHA-256 of the file on disk is compared
    /// directly against `expected_sha`.  For `Tgz` artifacts (e.g. `docker`)
    /// the expected hash refers to the *archive*, not the extracted binary, so
    /// we store/read a sidecar `{name}.sha256` file instead.
    async fn is_cached(&self, name: &str, expected_sha: &str, format: ArtifactFormat) -> bool {
        match format {
            ArtifactFormat::Binary => {
                let path = self.install_dir.join(name);
                sha256_file(&path)
                    .await
                    .map(|actual| actual == expected_sha)
                    .unwrap_or(false)
            }
            ArtifactFormat::Tgz => {
                let sidecar = self.install_dir.join(format!("{name}.sha256"));
                tokio::fs::read_to_string(&sidecar)
                    .await
                    .map(|content| content.trim() == expected_sha)
                    .unwrap_or(false)
            }
        }
    }

    /// Try to install a tool from the app bundle directory.
    ///
    /// Returns `true` if the tool was successfully installed from the bundle.
    /// Writes to a temporary file first, then atomically renames to `dest`.
    async fn try_install_from_bundle(
        &self,
        tool: &ToolEntry,
        dest: &Path,
        expected_sha: &str,
        format: ArtifactFormat,
    ) -> Result<bool, DockerToolError> {
        let bundle_dir = match &self.bundle_dir {
            Some(dir) => dir,
            None => return Ok(false),
        };

        let src = bundle_dir.join(&tool.name);
        if !src.exists() {
            return Ok(false);
        }

        // Write to a temp file to avoid following symlinks at dest.
        let tmp = dest.with_extension("bundle.tmp");
        if let Err(e) = tokio::fs::copy(&src, &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            info!(tool = %tool.name, error = %e, "bundle copy failed, will download");
            return Ok(false);
        }

        // For Binary format, verify the SHA-256 against assets.lock.
        // For Tgz format, expected_sha is for the archive — trust code-signed
        // app bundle instead.
        if format == ArtifactFormat::Binary {
            match sha256_file(&tmp).await {
                Ok(actual) if actual == expected_sha => {}
                Ok(_) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    info!(tool = %tool.name, "bundle checksum mismatch, will download");
                    return Ok(false);
                }
                Err(_) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    info!(tool = %tool.name, "bundle checksum read failed, will download");
                    return Ok(false);
                }
            }
        }

        // Atomic rename into place.
        if let Err(e) = tokio::fs::rename(&tmp, dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            info!(tool = %tool.name, error = %e, "bundle rename failed, will download");
            return Ok(false);
        }

        Ok(true)
    }

    /// Validate that all tools are installed and checksums match.
    pub async fn validate_all(&self) -> Result<(), DockerToolError> {
        for tool in &self.tools {
            let expected_sha = tool.sha256_for_arch(&self.arch).ok_or_else(|| {
                DockerToolError::UnsupportedArch {
                    tool: tool.name.clone(),
                    arch: self.arch.clone(),
                }
            })?;

            let path = self.install_dir.join(&tool.name);
            if !path.exists() {
                return Err(DockerToolError::NotInstalled(tool.name.clone()));
            }

            let format = registry::artifact_format(&tool.name);
            match format {
                ArtifactFormat::Binary => {
                    let actual = sha256_file(&path).await.map_err(DockerToolError::Asset)?;
                    if actual != expected_sha {
                        return Err(DockerToolError::Asset(
                            arcbox_asset::AssetError::ChecksumMismatch {
                                name: tool.name.clone(),
                                expected: expected_sha.to_string(),
                                actual,
                            },
                        ));
                    }
                }
                ArtifactFormat::Tgz => {
                    let sidecar = self.install_dir.join(format!("{}.sha256", tool.name));
                    let content = tokio::fs::read_to_string(&sidecar)
                        .await
                        .map_err(DockerToolError::Io)?;
                    if content.trim() != expected_sha {
                        return Err(DockerToolError::Asset(
                            arcbox_asset::AssetError::ChecksumMismatch {
                                name: tool.name.clone(),
                                expected: expected_sha.to_string(),
                                actual: content.trim().to_string(),
                            },
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns the install directory.
    #[must_use]
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    /// Returns the list of tool entries.
    #[must_use]
    pub fn tools(&self) -> &[ToolEntry] {
        &self.tools
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Mark a file as executable (0o755).
#[cfg(unix)]
async fn mark_executable(path: &Path) -> Result<(), DockerToolError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(path)
        .await
        .map_err(DockerToolError::Io)?
        .permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(path, perms)
        .await
        .map_err(DockerToolError::Io)?;
    Ok(())
}

#[cfg(not(unix))]
async fn mark_executable(_path: &Path) -> Result<(), DockerToolError> {
    Ok(())
}

/// Write a sidecar `{name}.sha256` file recording the expected checksum from
/// `assets.lock`.  Used for tgz-based tools where the on-disk binary cannot be
/// compared against the archive checksum directly.
async fn write_sidecar(
    install_dir: &Path,
    name: &str,
    expected_sha: &str,
) -> Result<(), DockerToolError> {
    let sidecar = install_dir.join(format!("{name}.sha256"));
    tokio::fs::write(&sidecar, expected_sha)
        .await
        .map_err(DockerToolError::Io)?;
    Ok(())
}

/// Extract a single file from a `.tgz` archive.
fn extract_from_tgz(
    archive_path: &Path,
    inner_path: &str,
    dest: &Path,
) -> Result<(), DockerToolError> {
    let file = std::fs::File::open(archive_path).map_err(DockerToolError::Io)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries().map_err(DockerToolError::Io)? {
        let mut entry = entry.map_err(DockerToolError::Io)?;
        let path = entry.path().map_err(DockerToolError::Io)?;
        if path.to_string_lossy() == inner_path {
            entry.unpack(dest).map_err(DockerToolError::Io)?;
            return Ok(());
        }
    }

    Err(DockerToolError::ExtractFailed {
        archive: archive_path.display().to_string(),
        inner: inner_path.to_string(),
    })
}

/// Errors from Docker tool operations.
#[derive(Debug, thiserror::Error)]
pub enum DockerToolError {
    #[error("asset error: {0}")]
    Asset(#[from] arcbox_asset::AssetError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tool '{tool}' has no binary for architecture '{arch}'")]
    UnsupportedArch { tool: String, arch: String },

    #[error("tool '{0}' is not installed")]
    NotInstalled(String),

    #[error("failed to extract '{inner}' from archive '{archive}'")]
    ExtractFailed { archive: String, inner: String },
}
