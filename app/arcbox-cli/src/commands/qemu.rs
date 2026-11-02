//! The QEMU build that emulates the System VM on a Mac without hardware
//! virtualization.

use std::path::PathBuf;

use anyhow::{Context, bail};
use arcbox_asset::AssetError;
use arcbox_image::ImageError;
use clap::{Args, Subcommand};

/// QEMU commands.
#[derive(Subcommand)]
pub enum QemuCommands {
    /// Install the pinned QEMU build from a local copy of its archive, for a
    /// Mac that cannot download it
    Import(ImportArgs),
}

/// Arguments for `abctl qemu import`.
#[derive(Args)]
pub struct ImportArgs {
    /// The archive, e.g. qemu-darwin-arm64-v0.1.0.tar.gz
    pub archive: PathBuf,
    /// Data directory to install into (default: the selected profile's)
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// Executes a QEMU command.
pub async fn execute(command: QemuCommands) -> anyhow::Result<()> {
    match command {
        QemuCommands::Import(args) => import(args).await,
    }
}

async fn import(args: ImportArgs) -> anyhow::Result<()> {
    let mut config = arcbox_core::Config::load()?;
    if let Some(data_dir) = args.data_dir {
        config.data_dir = data_dir;
    }
    let Some(runtime) = config.qemu_runtime()? else {
        bail!(
            "this ArcBox version does not use a QEMU build on this Mac; there is nothing to import"
        );
    };
    let artifact = runtime.artifact();
    let executable = match runtime.import(&args.archive).await {
        Ok(executable) => executable,
        Err(ImageError::Asset(AssetError::ChecksumMismatch { actual, .. })) => bail!(
            "{} is not the QEMU {} archive this ArcBox version needs: its SHA-256 is {actual}, \
             expected {} ({} from {})",
            args.archive.display(),
            artifact.version,
            artifact.sha256,
            artifact.archive_name(),
            artifact.url()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to import {}", args.archive.display()));
        }
    };
    let installed = executable
        .ancestors()
        .nth(2)
        .unwrap_or(&executable)
        .display();
    println!("Installed QEMU {} in {installed}", artifact.version);
    Ok(())
}
