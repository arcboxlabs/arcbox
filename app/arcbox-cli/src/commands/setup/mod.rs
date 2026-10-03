//! Shell integration setup commands.
//!
//! Manages CLI registration in the user's effective login environment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arcbox_constants::paths::HostLayout;
use clap::{Subcommand, ValueEnum};

use super::OutputFormat;

mod completions;
mod install;
mod profile;
mod status;

pub(super) use install::remove_integration;
pub(super) use status::{ComponentStatus, shell_integration_status};

/// The files `setup install` writes, and the shell profile it sources them
/// from, resolved once.
///
/// `abctl uninstall` builds one from its own view of the home and data
/// directories instead of reading the environment again.
pub(super) struct Integration {
    /// `<data_dir>/bin`: the `abctl` and Docker tool symlinks.
    pub(super) bin: PathBuf,
    /// `<data_dir>/shell`: the init scripts.
    pub(super) shell: PathBuf,
    /// `<data_dir>/completions`.
    pub(super) completions: PathBuf,
    /// The shell profile that sources the init script.
    pub(super) profile: PathBuf,
    pub(super) shell_kind: ShellKind,
    /// The Docker config directory holding `cli-plugins/` and `config.json`.
    pub(super) docker_config: PathBuf,
}

impl Integration {
    pub(super) async fn from_env() -> Result<Self> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Self::under(
            &home,
            &arcbox_home(),
            super::cli_plugins::default_docker_config_dir()?,
        )
        .await
    }

    /// The integration `setup install` lays out for `home` and `data_dir`.
    pub(super) async fn under(
        home: &Path,
        data_dir: &Path,
        docker_config: PathBuf,
    ) -> Result<Self> {
        let shell_kind = profile::detect_shell();
        Ok(Self {
            bin: data_dir.join("bin"),
            shell: data_dir.join("shell"),
            completions: data_dir.join("completions"),
            profile: profile::profile_path_under(shell_kind, home).await?,
            shell_kind,
            docker_config,
        })
    }
}

/// Shell integration setup commands.
#[derive(Subcommand)]
pub enum SetupCommands {
    /// Install shell integration (PATH, completions, profile)
    Install,

    /// Remove shell integration
    Uninstall,

    /// Check installation status
    Status,

    /// Print shell completions to stdout
    Completions(CompletionsArgs),
}

/// Arguments for the completions subcommand.
#[derive(clap::Args)]
pub struct CompletionsArgs {
    /// Target shell
    #[arg(long, value_enum)]
    pub shell: ShellKind,
}

/// Supported shells.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl ShellKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }
}

/// Execute setup commands.
pub async fn execute(command: SetupCommands, format: OutputFormat) -> Result<()> {
    match command {
        SetupCommands::Install => install::install(format).await,
        SetupCommands::Uninstall => install::uninstall(format).await,
        SetupCommands::Status => status::status(format).await,
        SetupCommands::Completions(args) => {
            completions::print(args.shell);
            Ok(())
        }
    }
}

fn arcbox_home() -> PathBuf {
    HostLayout::from_env_or_default().data_dir
}

fn bin_dir() -> PathBuf {
    arcbox_home().join("bin")
}

fn shell_dir() -> PathBuf {
    arcbox_home().join("shell")
}

fn completions_dir() -> PathBuf {
    arcbox_home().join("completions")
}
