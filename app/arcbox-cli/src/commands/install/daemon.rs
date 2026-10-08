//! User daemon registration and login-user resolution for installation.

use std::process::Command;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::ArcboxProfile;

/// Registers the selected user's daemon as a launchd user agent.
pub(super) fn register_daemon_service(target: &super::target::Target) -> Result<()> {
    let (home, uid, data_dir) = (&target.home, target.uid, &target.data_dir);
    let profile = ArcboxProfile::from_env_or_default();
    let daemon_label = profile.daemon_label();
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

    let daemon_path = super::super::bundle::locate_daemon(data_dir)?
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
