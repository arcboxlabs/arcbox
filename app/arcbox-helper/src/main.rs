//! arcbox-helper — minimal setuid-capable binary for privileged host mutations.
//!
//! This binary replaces the Swift ArcBoxHelper. It handles ONLY privileged
//! mutations (routes, DNS resolver files, socket symlinks) — no intelligence,
//! no scanning. The daemon tells it exactly what to do.
//!
//! All output is JSON on stdout. Exit code 0 = success, 1 = error.

mod dns;
mod route;
mod socket;
mod validate;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arcbox-helper")]
#[command(about = "Privileged helper for ArcBox host mutations")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage host routes
    #[command(subcommand)]
    Route(RouteCommands),

    /// Manage DNS resolver files
    #[command(subcommand)]
    Dns(DnsCommands),

    /// Manage Docker socket symlink
    #[command(subcommand)]
    Socket(SocketCommands),

    /// Print version
    Version,
}

#[derive(Subcommand)]
enum RouteCommands {
    /// Add a route for a subnet via a bridge interface
    Add {
        /// Subnet in CIDR notation (private ranges only)
        #[arg(long)]
        subnet: String,
        /// Bridge interface name (e.g. bridge100)
        #[arg(long)]
        iface: String,
    },
    /// Remove a route for a subnet
    Remove {
        /// Subnet in CIDR notation
        #[arg(long)]
        subnet: String,
    },
}

#[derive(Subcommand)]
enum DnsCommands {
    /// Install a DNS resolver file
    Install {
        /// Domain to resolve (e.g. arcbox.local)
        #[arg(long)]
        domain: String,
        /// Port the DNS server listens on
        #[arg(long)]
        port: u16,
    },
    /// Remove a DNS resolver file
    Uninstall {
        /// Domain to uninstall
        #[arg(long)]
        domain: String,
    },
    /// Check if a DNS resolver file is installed
    Status {
        /// Domain to check
        #[arg(long)]
        domain: String,
    },
}

#[derive(Subcommand)]
enum SocketCommands {
    /// Create /var/run/docker.sock symlink
    Link {
        /// Target socket path (must be under ~/.arcbox/)
        #[arg(long)]
        target: String,
    },
    /// Remove /var/run/docker.sock symlink
    Unlink,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Route(RouteCommands::Add { subnet, iface }) => route::add(&subnet, &iface),
        Commands::Route(RouteCommands::Remove { subnet }) => route::remove(&subnet),
        Commands::Dns(DnsCommands::Install { domain, port }) => dns::install(&domain, port),
        Commands::Dns(DnsCommands::Uninstall { domain }) => dns::uninstall(&domain),
        Commands::Dns(DnsCommands::Status { domain }) => match dns::status(&domain) {
            Ok(installed) => {
                print_json(true, installed);
                return;
            }
            Err(e) => Err(e),
        },
        Commands::Socket(SocketCommands::Link { target }) => socket::link(&target),
        Commands::Socket(SocketCommands::Unlink) => socket::unlink(),
        Commands::Version => {
            print_json_version();
            return;
        }
    };

    match result {
        Ok(()) => print_ok(),
        Err(e) => {
            print_err(&e);
            std::process::exit(1);
        }
    }
}

/// Prints `{"ok": true}` to stdout.
fn print_ok() {
    println!("{}", serde_json::json!({"ok": true}));
}

/// Prints `{"ok": false, "error": "..."}` to stdout.
fn print_err(error: &str) {
    println!("{}", serde_json::json!({"ok": false, "error": error}));
}

/// Prints `{"ok": true, "installed": bool}` for dns status.
fn print_json(ok: bool, installed: bool) {
    println!("{}", serde_json::json!({"ok": ok, "installed": installed}));
}

/// Prints version info as JSON.
fn print_json_version() {
    println!(
        "{}",
        serde_json::json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})
    );
}
