//! User daemon registration and login-user resolution for installation.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::ArcboxProfile;

/// Registers the daemon as a launchd user agent.
///
/// Under `sudo`, `dirs::home_dir()` returns `/var/root` and `getuid()`
/// returns 0. We detect this via `SUDO_USER` / `SUDO_UID` and resolve
/// the real user's home and UID instead.
pub(super) fn register_daemon_service() -> Result<()> {
    let (home, uid) = resolve_real_user()?;
    let profile = ArcboxProfile::from_env_or_default();
    let daemon_label = profile.daemon_label();
    let data_dir = std::env::var(arcbox_constants::env::DATA_DIR)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(
            || match profile {
                ArcboxProfile::Production => home.join(".arcbox"),
                ArcboxProfile::Development => home.join(".arcbox-dev"),
            },
            PathBuf::from,
        );
    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir).context("failed to create LaunchAgents directory")?;

    // Create log directory so launchd's stdout/stderr redirection works
    // on fresh installs where the profile log directory doesn't exist yet.
    let log_dir = data_dir.join("log");
    std::fs::create_dir_all(&log_dir).context("failed to create log directory")?;

    // Under sudo, the directory is created as root. Chown to the real user
    // so the daemon (running as user) can write logs.
    let _ = Command::new("chown")
        .args(["-R", &format!("{uid}:staff"), &data_dir.to_string_lossy()])
        .status();

    let plist_path = plist_dir.join(format!("{daemon_label}.plist"));

    let daemon_path = super::super::bundle::locate_daemon(&data_dir)?
        .to_string_lossy()
        .into_owned();

    // Daemon manages its own log files via tracing-appender, so we no
    // longer set StandardOutPath / StandardErrorPath. Stdout/stderr are
    // discarded to avoid launchd writing duplicate output.
    let data_dir_env = arcbox_constants::env::DATA_DIR;
    let data_dir_str = data_dir.display();
    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{daemon_label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{daemon_path}</string>
        <string>--profile</string>
        <string>{profile}</string>
        <string>--data-dir</string>
        <string>{data_dir_str}</string>
    </array>
    <!--
      Mirror the data dir into the daemon environment: recovery's host-global
      self-setup treats {data_dir_env} as the machine-wide canonical layout,
      so a relocated install keeps ownership of /var/run/docker.sock (the
      argv flag alone reads as a scoped per-instance override there).
    -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>{data_dir_env}</key>
        <string>{data_dir_str}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <!--
      ExitTimeOut: seconds launchd waits after SIGTERM before sending SIGKILL.
      Daemon shutdown can use all 45s (two 5s NFS unmount attempts + 5s service
      drain + 30s VM graceful stop). This preserves the full VM stop budget;
      only final best-effort cleanup may run up against the deadline.
    -->
    <key>ExitTimeOut</key>
    <integer>45</integer>
</dict>
</plist>
"#
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
