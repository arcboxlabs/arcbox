//! CLI command implementations.
//!
//! This module contains all the command handlers for the ArcBox CLI.
//! Commands are organized into:
//!
//! - Daemon lifecycle management
//! - Machine management
//! - Runtime migration
//! - Boot asset management
//! - Docker CLI integration
//! - DNS resolver management
//! - System information and version output

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub mod boot;
pub mod daemon;
#[cfg(target_os = "macos")]
pub mod dns;
pub mod docker;
pub mod doctor;
#[cfg(target_os = "macos")]
pub mod install;
pub mod logs;
pub mod machine;
pub mod migrate;
pub mod sandbox;
pub mod setup;
#[cfg(target_os = "macos")]
pub mod uninstall;
pub mod version;

/// ArcBox - High-performance container and VM runtime
#[derive(Parser)]
#[command(name = "abctl")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Command to execute
    #[command(subcommand)]
    pub command: Commands,

    /// Unix socket path for daemon connection
    ///
    /// Can also be set via ARCBOX_SOCKET or DOCKER_HOST environment variables.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Output format
    #[arg(long, global = true, default_value = "table")]
    pub format: OutputFormat,

    /// Enable debug output
    #[arg(long, global = true)]
    pub debug: bool,
}

/// Output format for command results.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    /// Table format (default)
    #[default]
    Table,
    /// JSON format
    Json,
    /// Quiet mode (IDs only)
    Quiet,
}

/// Available commands
#[derive(Subcommand)]
pub enum Commands {
    /// Manage Linux machines
    #[command(subcommand)]
    Machine(machine::MachineCommands),

    /// Import workloads from Docker Desktop or OrbStack
    #[command(subcommand)]
    Migrate(migrate::MigrateCommands),

    /// Manage sandboxes inside a machine
    #[command(subcommand)]
    Sandbox(sandbox::SandboxCommands),

    /// Manage Docker CLI integration
    #[command(subcommand)]
    Docker(docker::DockerCommands),

    /// Manage boot assets (kernel/rootfs)
    #[command(subcommand)]
    Boot(boot::BootCommands),

    /// Manage DNS resolver for *.arcbox.local
    #[cfg(target_os = "macos")]
    #[command(subcommand)]
    Dns(dns::DnsCommands),

    /// Manage the ArcBox daemon
    Daemon(daemon::DaemonArgs),

    /// View component logs
    Logs(logs::LogsArgs),

    /// Manage CLI shell integration (PATH, completions)
    #[command(subcommand)]
    Setup(setup::SetupCommands),

    /// Run diagnostic checks on the ArcBox runtime
    Doctor,

    /// Internal: install helper + register daemon (used by brew/DMG installers)
    #[cfg(target_os = "macos")]
    #[command(name = "_install", hide = true)]
    Install(install::InstallArgs),

    /// Internal: uninstall helper + deregister daemon (used by brew/DMG installers)
    #[cfg(target_os = "macos")]
    #[command(name = "_uninstall", hide = true)]
    Uninstall(uninstall::UninstallArgs),

    /// Display system-wide information
    Info,

    /// Show version information
    Version,
}
