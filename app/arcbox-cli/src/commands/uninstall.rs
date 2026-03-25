//! Complete uninstall of ArcBox from the system.
//!
//! Removes daemon, helper, system files, data, app bundle, and finally the
//! CLI binary itself. Requires interactive confirmation and sudo for
//! privileged operations.

use anyhow::{Context, Result};
use arcbox_constants::paths::{DOCKER_CLI_TOOLS, privileged};
use clap::Args;
use std::io::Write;
use std::process::Command;

/// Uninstall ArcBox from this machine.
#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Skip confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Preserve container data (~/.arcbox/data).
    #[arg(long)]
    pub keep_data: bool,
}

pub async fn execute(args: UninstallArgs) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let data_dir = home.join(".arcbox");

    println!("This will remove ArcBox and all its data:\n");
    println!("  • Stop and remove daemon (LaunchAgent)");
    println!("  • Stop and remove helper (binary, plist, socket)  [sudo]");
    println!("  • Remove DNS resolver (/etc/resolver/arcbox.local) [sudo]");
    println!("  • Remove Docker socket (/var/run/docker.sock)    [sudo]");
    println!("  • Remove CLI symlinks (/usr/local/bin/docker...) [sudo]");
    println!("  • Remove Docker context 'arcbox'");
    if args.keep_data {
        println!("  • Remove app data (~/.arcbox) — keeping container data");
    } else {
        println!("  • Remove ALL app data (~/.arcbox) including containers");
    }
    println!("  • Remove app (/Applications/ArcBox Desktop.app)");
    println!("  • Reset login item approvals (System Settings)   [sudo]");
    println!();

    if !args.yes {
        print!("Continue? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Cache sudo credentials up front so the user only enters password once.
    println!();
    let sudo_ok = Command::new("sudo").args(["-v"]).status().is_ok();
    if !sudo_ok {
        anyhow::bail!("sudo authentication failed");
    }

    let mut step = 0u32;
    let total = 10u32;

    macro_rules! step {
        ($label:expr, $body:expr) => {
            step += 1;
            print!("[{step}/{total}] {:<42}", $label);
            std::io::stdout().flush().ok();
            let result: std::result::Result<(), String> = {
                $body;
                Ok(())
            };
            match result {
                Ok(()) => println!("✓"),
                Err(e) => println!("✗ {e}"),
            }
        };
    }

    // 1. Quit the app.
    step!("Quitting ArcBox Desktop...", {
        let _ = Command::new("osascript")
            .args(["-e", r#"quit app "ArcBox Desktop""#])
            .output();
        // Wait for app to quit and daemon to stop.
        std::thread::sleep(std::time::Duration::from_secs(3));
    });

    // 2. Stop daemon.
    step!("Stopping daemon...", {
        let uid = unsafe { libc::getuid() };
        let _ = Command::new("launchctl")
            .args([
                "bootout",
                &format!("gui/{uid}/com.arcboxlabs.desktop.daemon"),
            ])
            .output();
        let _ = Command::new("pkill")
            .args(["-f", "com.arcboxlabs.desktop.daemon"])
            .output();
        // Wait for VM processes to exit gracefully.
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = Command::new("pkill")
            .args(["-f", "com.apple.Virtualization.VirtualMachine"])
            .output();
    });

    // 3. Stop and remove helper.
    step!("Removing helper...                  [sudo]", {
        let _ = Command::new("sudo")
            .args([
                "launchctl",
                "bootout",
                "system/com.arcboxlabs.desktop.helper",
            ])
            .output();
        let _ = Command::new("sudo")
            .args(["pkill", "-f", "arcbox-helper"])
            .output();
        let _ = Command::new("sudo")
            .args(["rm", "-f", privileged::HELPER_BINARY])
            .output();
        let _ = Command::new("sudo")
            .args(["rm", "-f", privileged::HELPER_PLIST])
            .output();
        let _ = Command::new("sudo")
            .args(["rm", "-f", privileged::HELPER_SOCKET])
            .output();
    });

    // 4. Remove DNS resolver.
    step!("Removing DNS resolver...            [sudo]", {
        let _ = Command::new("sudo")
            .args(["rm", "-f", "/etc/resolver/arcbox.local"])
            .output();
    });

    // 5. Remove Docker socket symlink.
    step!("Removing Docker socket...           [sudo]", {
        if let Ok(target) = std::fs::read_link(privileged::DOCKER_SOCKET) {
            if target.to_string_lossy().contains(".arcbox") {
                let _ = Command::new("sudo")
                    .args(["rm", "-f", privileged::DOCKER_SOCKET])
                    .output();
            }
        }
    });

    // 6. Remove CLI and Docker symlinks.
    step!("Removing CLI symlinks...            [sudo]", {
        // Remove abctl symlink if it points to ArcBox.
        if let Ok(target) = std::fs::read_link("/usr/local/bin/abctl") {
            if target.to_string_lossy().contains("ArcBox") {
                let _ = Command::new("sudo")
                    .args(["rm", "-f", "/usr/local/bin/abctl"])
                    .output();
            }
        }
        // Remove Docker CLI symlinks created by helper cli_link.
        for name in DOCKER_CLI_TOOLS {
            let path = format!("/usr/local/bin/{name}");
            if let Ok(target) = std::fs::read_link(&path) {
                if target
                    .to_string_lossy()
                    .contains(".app/Contents/MacOS/xbin/")
                {
                    let _ = Command::new("sudo").args(["rm", "-f", &path]).output();
                }
            }
        }
    });

    // 7. Remove Docker context.
    step!("Removing Docker context...", {
        let _ = Command::new("docker")
            .args(["context", "rm", "arcbox"])
            .output();
        // Restore default context if arcbox was active.
        let _ = Command::new("docker")
            .args(["context", "use", "default"])
            .output();
    });

    // 8. Remove data.
    step!("Removing data...", {
        if args.keep_data {
            // Remove everything except data/
            if let Ok(entries) = std::fs::read_dir(&data_dir) {
                for entry in entries.flatten() {
                    if entry.file_name() == "data" {
                        continue;
                    }
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        } else {
            let _ = std::fs::remove_dir_all(&data_dir);
        }
    });

    // 9. Remove app bundle.
    step!("Removing app...", {
        let _ = std::fs::remove_dir_all("/Applications/ArcBox Desktop.app");
    });

    // 10. Reset login item approvals (SMAppService / BTM database).
    //     This clears the "Allow in the Background" toggles in System
    //     Settings → General → Login Items for both the daemon and helper,
    //     so a reinstall starts with a clean approval state.
    step!("Resetting login item approvals...   [sudo]", {
        let _ = Command::new("sudo").args(["sfltool", "resetbtm"]).output();
    });

    println!("\nArcBox has been uninstalled.");
    if args.keep_data {
        println!("Container data preserved at {}/data", data_dir.display());
    }

    Ok(())
}
