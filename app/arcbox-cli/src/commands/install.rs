//! One-time privileged setup for ArcBox (CLI-only users).
//!
//! Installs the helper binary (requires sudo) and registers the daemon as
//! a launchd user agent. Everything else — DNS resolver, Docker socket,
//! boot assets, runtime binaries, Docker CLI tools — is handled by the
//! daemon during startup via self-setup.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use arcbox_constants::paths::{ArcboxProfile, privileged};

mod daemon;
mod preflight;
mod target;

/// Arguments for the install command.
#[derive(clap::Args)]
pub struct InstallArgs {
    /// Inspect helper readiness and selected legacy resolvers without changing state.
    #[arg(long)]
    pub check: bool,

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

    /// Login-user ID for the selected daemon and data directory.
    #[arg(long)]
    pub user_uid: Option<u32>,

    /// Absolute data directory for the selected daemon.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Exact legacy resolver roots to migrate; replaces the profile default.
    #[arg(long)]
    pub resolver_root: Vec<String>,
}

/// Executes the install command.
pub async fn execute(args: InstallArgs) -> Result<()> {
    let profile = ArcboxProfile::from_env_or_default();
    validate_install_scope(profile, &args)?;
    let target = target::Target::resolve(args.user_uid, args.data_dir)?;
    let roots = resolver_roots(profile, args.resolver_root);
    let helper = resolve_helper_source(args.helper_path.as_deref())?;
    let status = preflight::inspect(&helper, &roots).await?;
    if args.check {
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    // SAFETY: geteuid has no arguments or failure case.
    let is_privileged = unsafe { libc::geteuid() } == 0;

    println!("ArcBox Install");
    println!("==============");
    println!();

    // 1. Install helper binary (requires sudo).
    print_step(1, 3, "Installing arcbox-helper...");
    if status.helper_ready && status.legacy_roots.is_empty() {
        print_skipped();
    } else {
        let stopped_daemon = target.hold_stopped_daemon()?;
        install_helper(&helper, &roots)?;
        let installed = preflight::inspect(&helper, &roots).await?;
        ensure!(
            installed.helper_ready && installed.legacy_roots.is_empty(),
            "helper installation did not reach a ready state: {installed:?}"
        );
        drop(stopped_daemon);
        print_done();
    }

    // 2. Register daemon service.
    print_step(2, 3, "Registering daemon service...");
    if args.no_daemon {
        print_skipped();
    } else {
        daemon::register_daemon_service(&target)?;
        print_done();
    }

    // 3. Shell integration.
    print_step(3, 3, "Setting up shell integration...");
    if args.no_shell {
        print_skipped();
    } else if is_privileged {
        println!("skipped (privileged installation)");
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

fn resolver_roots(profile: ArcboxProfile, roots: Vec<String>) -> Vec<String> {
    if roots.is_empty() && profile == ArcboxProfile::Production {
        vec![arcbox_constants::dns::LOCAL_DOMAIN.to_owned()]
    } else {
        roots
    }
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

fn resolve_helper_source(custom_path: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(path) = custom_path {
        path.to_owned()
    } else {
        let exe = super::bundle::current_executable()?;
        exe.parent()
            .context("executable has no parent directory")?
            .join("arcbox-helper")
    };
    path.canonicalize()
        .with_context(|| format!("resolve helper source {}", path.display()))
}

/// Installs the arcbox-helper binary and registers it as a launchd system
/// daemon with socket activation.
///
/// launchd creates the socket at `/var/run/arcbox-helper.sock` and starts
/// the helper on-demand when the main daemon connects.
fn install_helper(helper_src: &Path, roots: &[String]) -> Result<()> {
    let dest = PathBuf::from(privileged::HELPER_BINARY);

    let destination_is_source = match dest.canonicalize() {
        Ok(path) => path == helper_src,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("resolve installed helper"),
    };
    let output = Command::new(helper_src)
        .arg("--prepare-install")
        .args(roots)
        .output()
        .context("failed to prepare helper installation")?;
    if !output.status.success() {
        bail!(
            "helper installation preparation failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Create target directory.
    std::fs::create_dir_all("/usr/local/libexec").context("failed to create /usr/local/libexec")?;

    // Copy binary.
    if !destination_is_source {
        std::fs::copy(helper_src, &dest).with_context(|| {
            format!(
                "failed to copy {} -> {}",
                helper_src.display(),
                dest.display()
            )
        })?;
    }

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
    use super::{InstallArgs, resolver_roots, validate_install_scope};
    use arcbox_constants::paths::ArcboxProfile;

    pub(super) fn isolated_lock_test(test: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "isolated lock test {test} failed ({}):\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn development_install_only_allows_the_helper_path() {
        let args = InstallArgs {
            check: false,
            no_daemon: true,
            no_shell: true,
            helper_path: None,
            user_uid: None,
            data_dir: None,
            resolver_root: Vec::new(),
        };
        assert!(validate_install_scope(ArcboxProfile::Development, &args).is_ok());

        for (no_daemon, no_shell) in [(false, true), (true, false), (false, false)] {
            let args = InstallArgs {
                check: false,
                no_daemon,
                no_shell,
                helper_path: None,
                user_uid: None,
                data_dir: None,
                resolver_root: Vec::new(),
            };
            assert!(validate_install_scope(ArcboxProfile::Development, &args).is_err());
            assert!(validate_install_scope(ArcboxProfile::Production, &args).is_ok());
        }
    }

    #[test]
    fn explicit_roots_replace_the_profile_migration_defaults() {
        assert_eq!(
            resolver_roots(ArcboxProfile::Production, Vec::new()),
            ["arcbox.local"]
        );
        assert_eq!(
            resolver_roots(ArcboxProfile::Development, Vec::new()),
            Vec::<String>::new()
        );
        for profile in [ArcboxProfile::Production, ArcboxProfile::Development] {
            let roots = vec!["custom.local".to_owned(), "other.local".to_owned()];
            assert_eq!(resolver_roots(profile, roots.clone()), roots);
        }
    }
}
