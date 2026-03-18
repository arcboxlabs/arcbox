//! One-time privileged setup for ArcBox.
//!
//! Replaces the desktop app's `StartupOrchestrator` — installs the helper
//! binary, DNS resolver, Docker socket symlink, daemon launchd service,
//! and shell integration in a single `sudo arcbox install` invocation.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Arguments for the install command.
#[derive(clap::Args)]
pub struct InstallArgs {
    /// Skip the daemon launchd service registration.
    #[arg(long)]
    pub no_daemon: bool,

    /// Skip shell integration setup.
    #[arg(long)]
    pub no_shell: bool,
}

/// Executes the install command.
pub async fn execute(args: InstallArgs) -> Result<()> {
    println!("ArcBox Install");
    println!("==============");
    println!();

    // 1. Install helper binary.
    print_step(1, 5, "Installing arcbox-helper...");
    install_helper()?;
    print_done();

    // 2. Install DNS resolver.
    print_step(2, 5, "Installing DNS resolver...");
    install_dns_resolver()?;
    print_done();

    // 3. Set up Docker socket.
    print_step(3, 5, "Setting up Docker socket...");
    setup_docker_socket()?;
    print_done();

    // 4. Register daemon service.
    print_step(4, 5, "Registering daemon service...");
    if args.no_daemon {
        print_skipped();
    } else {
        register_daemon_service()?;
        print_done();
    }

    // 5. Shell integration.
    print_step(5, 5, "Setting up shell integration...");
    if args.no_shell {
        print_skipped();
    } else {
        // Delegate to `arcbox setup install` logic.
        super::setup::execute(
            super::setup::SetupCommands::Install,
            super::OutputFormat::Quiet,
        )
        .await?;
        print_done();
    }

    println!();
    println!("ArcBox installed. The daemon will start automatically.");

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

// =============================================================================
// Step 1: Install helper binary + launchd service
// =============================================================================

/// Helper binary install path.
const HELPER_DEST: &str = "/usr/local/libexec/arcbox-helper";

/// launchd plist path (system-level daemon, runs as root).
const HELPER_PLIST: &str = "/Library/LaunchDaemons/com.arcboxlabs.desktop.helper.plist";

/// Installs the arcbox-helper binary and registers it as a launchd system
/// daemon with socket activation.
///
/// launchd creates the socket at `/var/run/arcbox-helper.sock` and starts
/// the helper on-demand when the main daemon connects.
fn install_helper() -> Result<()> {
    let dest = PathBuf::from(HELPER_DEST);

    // Find the helper binary next to our own executable.
    let exe = std::env::current_exe().context("could not determine current executable")?;
    let exe_dir = exe.parent().context("executable has no parent directory")?;
    let helper_src = exe_dir.join("arcbox-helper");

    if !helper_src.exists() {
        bail!(
            "arcbox-helper not found at {}. Build it first with: cargo build -p arcbox-helper",
            helper_src.display()
        );
    }

    // Create target directory.
    std::fs::create_dir_all("/usr/local/libexec")
        .context("failed to create /usr/local/libexec")?;

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
        .args(["root:wheel", HELPER_DEST])
        .status()
        .context("failed to chown helper binary")?;
    if !status.success() {
        bail!("chown root:wheel failed (are you running with sudo?)");
    }

    // Install bundled launchd plist (socket activation config is static).
    std::fs::write(
        HELPER_PLIST,
        include_bytes!("../../../../bundle/com.arcboxlabs.desktop.helper.plist"),
    )
    .with_context(|| format!("failed to write {HELPER_PLIST}"))?;

    // Bootout existing service (ignore errors if not loaded).
    let _ = Command::new("launchctl")
        .args(["bootout", "system", HELPER_PLIST])
        .output();

    // Bootstrap the service into the system domain.
    let status = Command::new("launchctl")
        .args(["bootstrap", "system", HELPER_PLIST])
        .status()
        .context("failed to run launchctl bootstrap")?;

    if !status.success() {
        bail!("launchctl bootstrap failed for helper service");
    }

    Ok(())
}

// =============================================================================
// Step 2: DNS resolver
// =============================================================================

/// Installs the DNS resolver file via the helper binary.
fn install_dns_resolver() -> Result<()> {
    let domain = dns_domain();
    let port = dns_port();

    let helper = find_helper()?;
    run_helper(
        &helper,
        &[
            "dns",
            "install",
            "--domain",
            &domain,
            "--port",
            &port.to_string(),
        ],
    )
}

// =============================================================================
// Step 3: Docker socket
// =============================================================================

/// Sets up the Docker socket symlink via the helper binary.
fn setup_docker_socket() -> Result<()> {
    let target = docker_socket_path();
    let target_str = target.to_string_lossy();

    let helper = find_helper();

    // If we just installed the helper, use it. Otherwise fall back to direct
    // symlink (we're presumably running as root via sudo).
    match helper {
        Ok(h) => run_helper(&h, &["socket", "link", "--target", &target_str]),
        Err(_) => {
            // Direct fallback when helper isn't found yet.
            let link = Path::new("/var/run/docker.sock");
            if let Ok(existing) = std::fs::read_link(link) {
                if existing == target {
                    return Ok(());
                }
                std::fs::remove_file(link).ok();
            }
            std::os::unix::fs::symlink(&target, link)
                .context("failed to create /var/run/docker.sock symlink")?;
            Ok(())
        }
    }
}

// =============================================================================
// Step 4: Daemon service
// =============================================================================

/// Registers the daemon as a launchd user agent.
fn register_daemon_service() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir).context("failed to create LaunchAgents directory")?;

    let plist_path = plist_dir.join("com.arcbox.daemon.plist");

    // Find the daemon binary.
    let exe = std::env::current_exe().context("could not determine current executable")?;
    let exe_dir = exe.parent().context("executable has no parent directory")?;
    let daemon_bin = exe_dir.join("arcbox-daemon");

    let daemon_path = if daemon_bin.exists() {
        daemon_bin.to_string_lossy().to_string()
    } else {
        // Fall back to ~/.arcbox/bin if not found next to CLI.
        let alt = home.join(".arcbox/bin/arcbox-daemon");
        if alt.exists() {
            alt.to_string_lossy().to_string()
        } else {
            "arcbox-daemon".to_string()
        }
    };

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.arcbox.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{daemon_path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{home}/.arcbox/log/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/.arcbox/log/daemon.err</string>
</dict>
</plist>
"#,
        home = home.display()
    );

    std::fs::write(&plist_path, plist_content)
        .with_context(|| format!("failed to write {}", plist_path.display()))?;

    // Bootstrap the service. Use launchctl bootstrap for the current user domain.
    let uid = unsafe { libc::getuid() };
    let domain_target = format!("gui/{uid}");

    // Bootout first to handle re-install (ignore errors if not loaded).
    let _ = Command::new("launchctl")
        .args(["bootout", &domain_target, &plist_path.to_string_lossy()])
        .output();

    let status = Command::new("launchctl")
        .args(["bootstrap", &domain_target, &plist_path.to_string_lossy()])
        .status()
        .context("failed to run launchctl bootstrap")?;

    if !status.success() {
        bail!("launchctl bootstrap failed");
    }

    Ok(())
}

// =============================================================================
// Helpers
// =============================================================================

fn dns_domain() -> String {
    std::env::var("ARCBOX_DNS_DOMAIN").unwrap_or_else(|_| "arcbox.local".to_string())
}

fn dns_port() -> u16 {
    std::env::var("ARCBOX_DNS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5553)
}

fn docker_socket_path() -> PathBuf {
    dirs::home_dir().map_or_else(
        || PathBuf::from("/var/run/arcbox-docker.sock"),
        |h| h.join(".arcbox/run/docker.sock"),
    )
}

/// Finds the arcbox-helper binary. Checks the just-installed location first,
/// then the locations route_reconciler would search.
fn find_helper() -> Result<PathBuf> {
    let candidates = [
        // Just installed by step 1.
        Some(PathBuf::from("/usr/local/libexec/arcbox-helper")),
        // Sibling of current exe.
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("arcbox-helper"))),
        // ~/.arcbox/bin
        dirs::home_dir().map(|h| h.join(".arcbox/bin/arcbox-helper")),
        Some(PathBuf::from("/usr/local/bin/arcbox-helper")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!("arcbox-helper not found")
}

/// Runs the helper binary with the given arguments and checks for success.
fn run_helper(helper: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new(helper)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {}", helper.display()))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "helper command failed: {}{}",
        stdout.trim(),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(" (stderr: {})", stderr.trim())
        }
    );
}
