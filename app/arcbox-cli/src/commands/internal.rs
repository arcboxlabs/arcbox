//! Internal commands for package manager integration.
//!
//! These commands are called by Homebrew Cask postflight/uninstall scripts
//! and are not intended for direct user invocation. They mirror OrbStack's
//! `orbctl _internal` pattern.
//!
//! Both hooks are idempotent and share the same code paths as the desktop
//! app's first-launch setup, so the result is identical regardless of
//! whether the user opens the app first or installs via `brew`.

use anyhow::{Context, Result};
use clap::Subcommand;

use super::OutputFormat;

/// Internal subcommands (hidden from help).
#[derive(Subcommand)]
pub enum InternalCommands {
    /// Homebrew Cask post-install hook.
    ///
    /// Runs the same first-launch setup that the desktop app performs:
    /// data directories, shell integration, and Docker CLI context.
    /// All operations are idempotent — if the app has already run,
    /// this is a no-op.
    #[command(name = "brew-postflight")]
    BrewPostflight,

    /// Homebrew Cask pre-uninstall hook.
    ///
    /// Stops the daemon, removes Docker context, and cleans up shell
    /// integration. Privileged components (helper, DNS resolver) require
    /// separate `sudo abctl _uninstall` cleanup.
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
/// Shares the same setup code paths as the desktop app's first launch.
/// Every step is idempotent — running after the app has already set up
/// is harmless.
async fn brew_postflight() -> Result<()> {
    // 1. Create data directories (same as daemon's init_early phase).
    let home = dirs::home_dir().context("could not determine home directory")?;
    let data_dir = home.join(".arcbox");
    for sub in ["run", "log", "data", "boot", "bin"] {
        tokio::fs::create_dir_all(data_dir.join(sub)).await?;
    }

    // 2. Shell integration — same code path as `abctl setup install`.
    super::setup::execute(super::setup::SetupCommands::Install, OutputFormat::Quiet).await?;

    // 3. Docker context — same DockerContextManager used by `abctl docker enable`
    //    and the daemon's self-setup. Non-fatal if Docker CLI isn't installed yet.
    if let Err(e) = setup_docker_context() {
        eprintln!("Note: Docker context setup skipped ({e})");
    }

    Ok(())
}

/// Pre-uninstall hook for Homebrew Cask.
///
/// Cleans up non-privileged, user-level state that the Cask `zap` stanza
/// does not cover. The existing `_uninstall` is too heavy here: it requires
/// sudo, removes the app bundle (Cask already does that), and deletes user
/// data (belongs to `brew zap`).
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

    // Remove the daemon plist so launchd doesn't try to restart.
    let plist = home.join("Library/LaunchAgents/com.arcboxlabs.desktop.daemon.plist");
    let _ = tokio::fs::remove_file(plist).await;

    // 2. Remove Docker context — same DockerContextManager as `abctl docker disable`.
    let _ = std::process::Command::new("docker")
        .args(["context", "rm", "-f", "arcbox"])
        .output();
    let _ = std::process::Command::new("docker")
        .args(["context", "use", "default"])
        .output();

    // 3. Remove shell integration — same code path as `abctl setup uninstall`.
    super::setup::execute(super::setup::SetupCommands::Uninstall, OutputFormat::Quiet).await?;

    Ok(())
}

/// Creates the 'arcbox' Docker context pointing at the ArcBox Docker socket.
/// Uses the same `DockerContextManager` as `abctl docker enable`.
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
