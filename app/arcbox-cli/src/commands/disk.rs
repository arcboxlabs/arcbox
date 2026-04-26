//! Disk management commands.
//!
//! Inspect and manage the Docker data disk image.

use anyhow::{Context, Result};
use clap::Subcommand;

/// Disk management commands.
#[derive(Subcommand)]
pub enum DiskCommands {
    /// Show disk usage for the Docker data image.
    Usage,
    /// Compact the Docker data image by trimming free blocks.
    Compact,
}

pub async fn execute(cmd: DiskCommands) -> Result<()> {
    match cmd {
        DiskCommands::Usage => execute_usage().await,
        DiskCommands::Compact => execute_compact().await,
    }
}

async fn execute_usage() -> Result<()> {
    let config = arcbox_core::Config::load().unwrap_or_default();
    let img_path = config.docker_img_path();

    if !img_path.exists() {
        println!("Docker data disk not found at {}", img_path.display());
        println!("The disk will be created when a machine is first started.");
        return Ok(());
    }

    let metadata = std::fs::metadata(&img_path)
        .with_context(|| format!("failed to stat {}", img_path.display()))?;

    let logical_bytes = metadata.len();

    #[cfg(unix)]
    let physical_bytes = {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks() * 512
    };
    #[cfg(not(unix))]
    let physical_bytes = logical_bytes;

    let logical_gib = logical_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let physical_gib = physical_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let available_gib =
        logical_bytes.saturating_sub(physical_bytes) as f64 / (1024.0 * 1024.0 * 1024.0);
    let usage_pct = if logical_bytes > 0 {
        (physical_bytes as f64 / logical_bytes as f64) * 100.0
    } else {
        0.0
    };

    println!("Docker data disk:");
    println!("  Path:      {}", img_path.display());
    println!("  Logical:   {logical_gib:.1} GiB");
    println!("  Physical:  {physical_gib:.1} GiB   ({usage_pct:.1}%)");
    println!("  Available: {available_gib:.1} GiB");

    Ok(())
}

async fn execute_compact() -> Result<()> {
    let config = arcbox_core::Config::load().unwrap_or_default();
    let img_path = config.docker_img_path();

    if !img_path.exists() {
        println!("Docker data disk not found at {}", img_path.display());
        return Ok(());
    }

    println!("Automatic disk compaction is enabled by default.");
    println!("The guest runs periodic fstrim and the host disk uses sparse allocation.");
    println!();
    println!("To reclaim space inside the VM, run:");
    println!("  docker system prune --all --volumes");
    println!();

    // Show current physical usage for reference.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&img_path)
            .with_context(|| format!("failed to stat {}", img_path.display()))?;
        let physical_gib = (m.blocks() * 512) as f64 / (1024.0 * 1024.0 * 1024.0);
        let logical_gib = m.len() as f64 / (1024.0 * 1024.0 * 1024.0);
        println!("Current: {physical_gib:.1} GiB physical / {logical_gib:.1} GiB logical");
    }

    Ok(())
}
