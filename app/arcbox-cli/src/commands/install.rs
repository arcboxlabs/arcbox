//! One-time privileged setup for ArcBox.
//!
//! Replaces the desktop app's `StartupOrchestrator` — installs the helper
//! binary, DNS resolver, Docker socket symlink, daemon launchd service,
//! and shell integration in a single `sudo arcbox install` invocation.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use arcbox_helper::client::Client;

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
    print_step(1, 6, "Installing arcbox-helper...");
    install_helper()?;
    print_done();

    // 2. Install DNS resolver via helper tarpc client.
    // The socket is available immediately after launchctl bootstrap because
    // launchd socket activation creates the socket file eagerly.
    print_step(2, 6, "Installing DNS resolver...");
    let client = Client::connect()
        .await
        .context("failed to connect to arcbox-helper (is the service running?)")?;
    install_dns_resolver(&client).await?;
    print_done();

    // 3. Set up Docker socket via helper tarpc client.
    print_step(3, 6, "Setting up Docker socket...");
    setup_docker_socket(&client).await?;
    print_done();

    // 4. Provision boot assets so the daemon can start without network.
    print_step(4, 6, "Provisioning boot assets...");
    provision_boot_assets().await?;
    print_done();

    // 5. Register daemon service.
    print_step(5, 6, "Registering daemon service...");
    if args.no_daemon {
        print_skipped();
    } else {
        register_daemon_service()?;
        print_done();
    }

    // 6. Shell integration.
    // Under sudo, dirs::home_dir() returns /var/root, so setup would
    // install to root's home instead of the real user's. Skip and hint.
    print_step(6, 6, "Setting up shell integration...");
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

/// Installs the DNS resolver file via the helper tarpc client.
async fn install_dns_resolver(client: &Client) -> Result<()> {
    let domain = dns_domain();
    let port = dns_port();

    client
        .dns_install(&domain, port)
        .await
        .context("failed to install DNS resolver via helper")?;

    Ok(())
}

// =============================================================================
// Step 3: Docker socket
// =============================================================================

/// Sets up the Docker socket symlink via the helper tarpc client.
async fn setup_docker_socket(client: &Client) -> Result<()> {
    let target = docker_socket_path();
    let target_str = target.to_string_lossy();

    client
        .socket_link(&target_str)
        .await
        .context("failed to create Docker socket symlink via helper")?;

    Ok(())
}

// =============================================================================
// Step 4: Boot asset provisioning
// =============================================================================

/// Downloads boot assets (kernel, rootfs) and runtime binaries so the daemon
/// can start without requiring a network connection.
async fn provision_boot_assets() -> Result<()> {
    use arcbox_core::boot_assets::{BootAssetConfig, BootAssetProvider};

    let (home, _) = resolve_real_user()?;
    let data_dir = home.join(".arcbox");
    let boot_cache_dir = data_dir.join("boot");

    let config = BootAssetConfig::with_cache_dir(boot_cache_dir);
    let provider = BootAssetProvider::with_config(config)?;

    // Download kernel + rootfs.
    provider
        .get_assets()
        .await
        .context("failed to download boot assets")?;

    // Download runtime binaries (dockerd, containerd, shim, runc).
    let runtime_bin_dir = data_dir.join("runtime/bin");
    std::fs::create_dir_all(&runtime_bin_dir).context("failed to create runtime bin directory")?;
    provider
        .prepare_binaries(&runtime_bin_dir, None)
        .await
        .context("failed to prepare runtime binaries")?;

    Ok(())
}

// =============================================================================
// Step 5: Daemon service
// =============================================================================

/// Registers the daemon as a launchd user agent.
///
/// Under `sudo`, `dirs::home_dir()` returns `/var/root` and `getuid()`
/// returns 0. We detect this via `SUDO_USER` / `SUDO_UID` and resolve
/// the real user's home and UID instead.
/// Daemon launchd label — must match uninstall.rs.
const DAEMON_LABEL: &str = "com.arcboxlabs.desktop.daemon";

fn register_daemon_service() -> Result<()> {
    let (home, uid) = resolve_real_user()?;
    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir).context("failed to create LaunchAgents directory")?;

    // Create log directory so launchd's stdout/stderr redirection works
    // on fresh installs where ~/.arcbox/log/ doesn't exist yet.
    let log_dir = home.join(".arcbox/log");
    std::fs::create_dir_all(&log_dir).context("failed to create log directory")?;

    // Under sudo, the directory is created as root. Chown to the real user
    // so the daemon (running as user) can write logs.
    let _ = Command::new("chown")
        .args([
            "-R",
            &format!("{uid}:staff"),
            &home.join(".arcbox").to_string_lossy(),
        ])
        .status();

    let plist_path = plist_dir.join(format!("{DAEMON_LABEL}.plist"));

    // Find the daemon binary.
    let exe = std::env::current_exe().context("could not determine current executable")?;
    let exe_dir = exe.parent().context("executable has no parent directory")?;
    let daemon_bin = exe_dir.join("arcbox-daemon");

    let daemon_path = if daemon_bin.exists() {
        daemon_bin.to_string_lossy().to_string()
    } else {
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
    <string>{DAEMON_LABEL}</string>
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

/// Resolves the real user's home directory and UID.
///
/// When running under `sudo`, `dirs::home_dir()` returns `/var/root` and
/// `libc::getuid()` returns 0. We use `SUDO_USER` to look up the home
/// directory and `SUDO_UID` for the UID. Falls back to the current process
/// values when not running under sudo.
fn resolve_real_user() -> Result<(PathBuf, u32)> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        let uid: u32 = std::env::var("SUDO_UID")
            .ok()
            .and_then(|s| s.parse().ok())
            .context("SUDO_USER is set but SUDO_UID is missing or invalid")?;

        // Resolve home directory from the password database.
        let home = home_for_user(&sudo_user)
            .unwrap_or_else(|| PathBuf::from(format!("/Users/{sudo_user}")));

        return Ok((home, uid));
    }

    let home = dirs::home_dir().context("could not determine home directory")?;
    // SAFETY: getuid() is a trivial POSIX syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    Ok((home, uid))
}

/// Looks up a user's home directory via the POSIX password database.
fn home_for_user(username: &str) -> Option<PathBuf> {
    let c_name = std::ffi::CString::new(username).ok()?;
    // SAFETY: getpwnam is a standard POSIX function. We pass a valid
    // null-terminated string and check the return value before
    // dereferencing. The returned pointer is to static storage.
    let pw = unsafe { libc::getpwnam(c_name.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    // SAFETY: pw is non-null and pw_dir is a valid C string.
    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
    Some(PathBuf::from(dir.to_string_lossy().into_owned()))
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
    // Use resolve_real_user() instead of dirs::home_dir() so that under
    // `sudo` we get the invoking user's home, not /var/root.
    resolve_real_user()
        .map(|(home, _uid)| home.join(".arcbox/run/docker.sock"))
        .unwrap_or_else(|_| PathBuf::from("/var/run/arcbox-docker.sock"))
}
