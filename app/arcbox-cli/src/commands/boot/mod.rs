//! Boot asset cache commands.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use super::OutputFormat;

mod cache;
mod status;

/// Boot asset commands and their cache location.
#[derive(Args)]
pub struct BootArgs {
    /// Data directory containing boot assets and runtime binaries.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    /// Boot asset operation.
    #[command(subcommand)]
    pub command: BootCommands,
}

/// Boot asset management commands.
#[derive(Subcommand)]
pub enum BootCommands {
    /// Download boot assets in advance
    Prefetch(PrefetchArgs),
    /// Show boot asset status
    Status(StatusArgs),
    /// Clear cached boot assets
    Clear,
    /// List cached versions
    List,
}

/// Arguments for prefetch command.
#[derive(Args)]
pub struct PrefetchArgs {
    /// Force re-download even if cached
    #[arg(long, short)]
    pub force: bool,
    /// Asset version to download (default: current version)
    #[arg(long = "asset-version")]
    pub asset_version: Option<String>,
}

/// Arguments for status command.
#[derive(Args)]
pub struct StatusArgs {
    /// Skip network request for latest version check
    #[arg(long)]
    pub offline: bool,
}

/// Execute boot commands.
pub async fn execute(args: BootArgs, format: OutputFormat) -> anyhow::Result<()> {
    let mut config = arcbox_core::Config::load()?;
    if let Some(data_dir) = args.data_dir {
        config.data_dir = data_dir;
    }
    let boot_config = config.boot_asset_config();
    let cache_dir = boot_config.cache_dir.clone();

    match args.command {
        BootCommands::Prefetch(args) => {
            cache::prefetch(&config.data_dir, boot_config, args, format).await
        }
        BootCommands::Status(args) => {
            status::status(&config.data_dir, boot_config, args, format).await
        }
        BootCommands::Clear => cache::clear(cache_dir, format).await,
        BootCommands::List => cache::list(cache_dir, format).await,
    }
}

#[cfg(test)]
mod tests {
    use arcbox_constants::paths::ArcboxProfile;
    use arcbox_image::boot_assets::BootAssetProvider;

    #[tokio::test]
    async fn development_profile_accepts_a_local_boot_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = arcbox_core::Config::for_profile(ArcboxProfile::Development);
        config.data_dir = directory.path().to_owned();
        let development = config.boot_asset_config();
        std::fs::create_dir_all(development.version_cache_dir()).unwrap();
        std::fs::write(
            development.version_cache_dir().join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 0,
                "asset_version": development.version.clone(),
                "built_at": "now",
                "targets": {},
                "binaries": []
            }))
            .unwrap(),
        )
        .unwrap();

        BootAssetProvider::with_config(development)
            .unwrap()
            .read_cached_manifest_required()
            .await
            .unwrap();

        config.profile = ArcboxProfile::Production;
        let error = BootAssetProvider::with_config(config.boot_asset_config())
            .unwrap()
            .read_cached_manifest_required()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("manifest SHA256 mismatch"));
    }
}
