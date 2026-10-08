//! Installs the daemon's bundled Docker CLI tools.

use std::path::Path;

/// Embedded lockfile (same one used by arcbox-core for boot assets).
const LOCK_TOML: &str = include_str!("../../../../assets.lock");

/// Installs Docker CLI tools (docker, buildx, compose, credential helper)
/// if not already present. Tries the app bundle first, then CDN.
pub(super) async fn ensure_docker_tools(data_dir: &Path) -> anyhow::Result<()> {
    let tools = arcbox_docker_tools::parse_tools_for_group(
        LOCK_TOML,
        arcbox_docker_tools::ToolGroup::Docker,
    )
    .map_err(|e| anyhow::anyhow!("failed to parse tools from assets.lock: {e}"))?;

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };

    let install_dir = data_dir.join("runtime/bin");
    let mgr = arcbox_docker_tools::HostToolManager::new(tools, arch, install_dir);

    // Docker tools are already seeded from the app bundle by
    // seed_from_bundle() (via Resources/runtime/bin/). This check
    // is a fast no-op when they're present and checksums match.
    if mgr.validate_all().await.is_ok() {
        tracing::debug!("Docker CLI tools already installed and valid");
        return Ok(());
    }

    mgr.install_all(None)
        .await
        .map_err(|e| anyhow::anyhow!("docker tools install failed: {e}"))?;

    tracing::info!("Docker CLI tools installed");
    Ok(())
}
