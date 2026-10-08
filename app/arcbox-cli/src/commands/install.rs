//! One-time privileged setup for ArcBox (CLI-only users).
//!
//! Installs the helper binary (requires sudo) and registers the daemon as
//! a launchd user agent. Everything else — DNS resolver, Docker socket,
//! boot assets, runtime binaries, Docker CLI tools — is handled by the
//! daemon during startup via self-setup.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::{ArcboxProfile, privileged};

mod daemon;

/// Arguments for the install command.
#[derive(clap::Args)]
pub struct InstallArgs {
    /// Skip the daemon launchd service registration.
    #[arg(long)]
    pub no_daemon: bool,

    /// Skip shell integration setup.
    #[arg(long)]
    pub no_shell: bool,

    /// Path to the arcbox-helper binary to install.
    /// Defaults to looking next to the current executable.
    #[arg(long)]
    pub helper_path: Option<PathBuf>,
}

/// Executes the install command.
pub async fn execute(args: InstallArgs) -> Result<()> {
    validate_install_scope(ArcboxProfile::from_env_or_default(), &args)?;

    println!("ArcBox Install");
    println!("==============");
    println!();

    // 1. Install helper binary (requires sudo).
    print_step(1, 3, "Installing arcbox-helper...");
    install_helper(args.helper_path.as_deref())?;
    print_done();

    // 2. Register daemon service.
    print_step(2, 3, "Registering daemon service...");
    if args.no_daemon {
        print_skipped();
    } else {
        daemon::register_daemon_service()?;
        print_done();
    }

    // 3. Shell integration.
    // Under sudo, dirs::home_dir() returns /var/root, so setup would
    // install to root's home instead of the real user's. Skip and hint.
    print_step(3, 3, "Setting up shell integration...");
    if args.no_shell {
        print_skipped();
    } else if std::env::var("SUDO_USER").is_ok() {
        println!("skipped (sudo)");
        println!();
        println!("  Run as your normal user: abctl setup install");
    } else {
        super::setup::execute(
            super::setup::SetupCommands::Install,
            super::OutputFormat::Quiet,
        )
        .await?;
        print_done();
    }

    println!();
    println!("ArcBox installed. The daemon will start automatically.");
    println!("DNS, Docker socket, and boot assets are configured on first daemon start.");

    Ok(())
}

fn validate_install_scope(profile: ArcboxProfile, args: &InstallArgs) -> Result<()> {
    if profile == ArcboxProfile::Development && (!args.no_daemon || !args.no_shell) {
        bail!(
            "Development installs may only update the shared helper; rerun with --no-daemon --no-shell. The Desktop app manages per-instance daemon and shell state."
        );
    }
    Ok(())
}

fn print_step(n: u32, total: u32, msg: &str) {
    print!("[{n}/{total}] {msg:<40}");
}

fn print_done() {
    println!("done");
}

fn print_skipped() {
    println!("skipped");
}

/// Installs the arcbox-helper binary and registers it as a launchd system
/// daemon with socket activation.
///
/// launchd creates the socket at `/var/run/arcbox-helper.sock` and starts
/// the helper on-demand when the main daemon connects.
fn install_helper(custom_path: Option<&std::path::Path>) -> Result<()> {
    let dest = PathBuf::from(privileged::HELPER_BINARY);

    // Use custom path if provided, otherwise look next to our own executable.
    let helper_src = if let Some(path) = custom_path {
        path.to_path_buf()
    } else {
        let exe = super::bundle::current_executable()?;
        let exe_dir = exe.parent().context("executable has no parent directory")?;
        exe_dir.join("arcbox-helper")
    };

    if !helper_src.exists() {
        bail!(
            "arcbox-helper not found at {}. Build it first with: cargo build -p arcbox-helper",
            helper_src.display()
        );
    }

    // Create target directory.
    std::fs::create_dir_all("/usr/local/libexec").context("failed to create /usr/local/libexec")?;

    // Copy binary.
    std::fs::copy(&helper_src, &dest).with_context(|| {
        format!(
            "failed to copy {} -> {}",
            helper_src.display(),
            dest.display()
        )
    })?;

    // Ensure root ownership (macOS copyfile preserves source ownership).
    let status = Command::new("chown")
        .args(["root:wheel", privileged::HELPER_BINARY])
        .status()
        .context("failed to chown helper binary")?;
    if !status.success() {
        bail!("chown root:wheel failed (are you running with sudo?)");
    }

    // Install bundled launchd plist (socket activation config is static).
    std::fs::write(
        privileged::HELPER_PLIST,
        include_bytes!("../../../../bundle/com.arcboxlabs.desktop.helper.plist"),
    )
    .with_context(|| format!("failed to write {}", privileged::HELPER_PLIST))?;

    // Bootout existing service (ignore errors if not loaded).
    let _ = Command::new("launchctl")
        .args(["bootout", "system", privileged::HELPER_PLIST])
        .output();

    // Bootstrap the service into the system domain.
    let status = Command::new("launchctl")
        .args(["bootstrap", "system", privileged::HELPER_PLIST])
        .status()
        .context("failed to run launchctl bootstrap")?;

    if !status.success() {
        bail!("launchctl bootstrap failed for helper service");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{InstallArgs, validate_install_scope};
    use arcbox_constants::paths::ArcboxProfile;

    #[test]
    fn development_install_only_allows_the_helper_path() {
        let args = InstallArgs {
            no_daemon: true,
            no_shell: true,
            helper_path: None,
        };
        assert!(validate_install_scope(ArcboxProfile::Development, &args).is_ok());

        for (no_daemon, no_shell) in [(false, true), (true, false), (false, false)] {
            let args = InstallArgs {
                no_daemon,
                no_shell,
                helper_path: None,
            };
            assert!(validate_install_scope(ArcboxProfile::Development, &args).is_err());
            assert!(validate_install_scope(ArcboxProfile::Production, &args).is_ok());
        }
    }
}
