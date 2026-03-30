//! Internal commands for package manager integration.
//!
//! These commands are called by Homebrew Cask postflight/uninstall scripts
//! and are not intended for direct user invocation. They mirror OrbStack's
//! `orbctl _internal` pattern.

use anyhow::{Context, Result};
use clap::Subcommand;

use super::OutputFormat;

/// Internal subcommands (hidden from help).
#[derive(Subcommand)]
pub enum InternalCommands {
    /// Homebrew Cask post-install hook.
    ///
    /// Sets up Docker CLI context, shell integration, and data directories.
    /// Runs as the current user (no root). Called automatically by
    /// `brew install --cask arcbox`.
    #[command(name = "brew-postflight")]
    BrewPostflight,

    /// Homebrew Cask pre-uninstall hook.
    ///
    /// Stops the daemon, removes Docker context, and cleans up shell
    /// integration. Called automatically by `brew uninstall --cask arcbox`.
    #[command(name = "brew-uninstall")]
    BrewUninstall,
}

pub async fn execute(cmd: InternalCommands) -> Result<()> {
    match cmd {
        InternalCommands::BrewPostflight => brew_postflight().await,
        InternalCommands::BrewUninstall => brew_uninstall().await,
    }
}

/// Post-install hook for Homebrew Cask.
///
/// Performs non-privileged setup that mirrors what the desktop app does on
/// first launch. Privileged helper installation is left to the app's
/// SMAppService authorization prompt.
async fn brew_postflight() -> Result<()> {
    // 1. Create data directories.
    let home = dirs::home_dir().context("could not determine home directory")?;
    let data_dir = home.join(".arcbox");
    for sub in ["run", "log", "data", "boot", "bin"] {
        tokio::fs::create_dir_all(data_dir.join(sub)).await?;
    }

    // 2. Shell integration (PATH + completions).
    super::setup::execute(super::setup::SetupCommands::Install, OutputFormat::Quiet).await?;

    // 3. Docker context (create 'arcbox' context and set as default).
    //    Errors are non-fatal — Docker CLI may not be installed yet.
    if let Err(e) = setup_docker_context() {
        eprintln!("Note: Docker context setup skipped ({e})");
    }

    Ok(())
}

/// Pre-uninstall hook for Homebrew Cask.
///
/// Cleans up user-level state. Privileged components (helper binary,
/// LaunchDaemon, DNS resolver) are left for `sudo abctl _uninstall` or
/// manual cleanup — Homebrew Cask uninstall scripts run as the current
/// user and cannot elevate.
async fn brew_uninstall() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;

    // 1. Stop the daemon via launchctl.
    // SAFETY: getuid() is a trivial POSIX syscall.
    let uid = unsafe { libc::getuid() };
    let _ = std::process::Command::new("launchctl")
        .args([
            "bootout",
            &format!("gui/{uid}/com.arcboxlabs.desktop.daemon"),
        ])
        .output();

    // Also remove the daemon plist so launchd doesn't try to restart.
    let plist = home.join("Library/LaunchAgents/com.arcboxlabs.desktop.daemon.plist");
    let _ = tokio::fs::remove_file(plist).await;

    // 2. Remove Docker context.
    let _ = std::process::Command::new("docker")
        .args(["context", "rm", "-f", "arcbox"])
        .output();
    let _ = std::process::Command::new("docker")
        .args(["context", "use", "default"])
        .output();

    // 3. Remove shell integration.
    super::setup::execute(super::setup::SetupCommands::Uninstall, OutputFormat::Quiet).await?;

    Ok(())
}

/// Creates the 'arcbox' Docker context pointing at the ArcBox Docker socket.
fn setup_docker_context() -> Result<()> {
    use super::docker;

    let manager = docker::context_manager()?;
    if !manager.context_exists() {
        manager.create_context()?;
    }
    if !manager.is_default()? {
        manager.set_default()?;
    }
    Ok(())
}
