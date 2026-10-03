//! The external commands an uninstall runs.
//!
//! One seam, so the tests drive the filesystem for real under temporary
//! roots and only record what would have reached launchd, `sudo`,
//! `osascript`, `security`, `defaults` and `umount`.

use std::ffi::OsStr;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

pub(super) trait Host: Sync {
    /// Runs `program args…` to completion. `Err` means it could not start.
    fn run(&self, program: &str, args: &[&OsStr]) -> Result<Output>;
}

/// The Mac this process runs on.
pub(super) struct Mac;

impl Host for Mac {
    fn run(&self, program: &str, args: &[&OsStr]) -> Result<Output> {
        Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("could not run {program}"))
    }
}

/// Runs a command and fails on a non-zero exit, quoting its stderr.
pub(super) fn run_checked(host: &dyn Host, program: &str, args: &[&OsStr]) -> Result<Output> {
    let output = host.run(program, args)?;
    if !output.status.success() {
        let shown: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        bail!(
            "{program} {} failed ({}): {}",
            shown.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// Runs `sudo args…`, failing on a non-zero exit.
pub(super) fn sudo(host: &dyn Host, args: &[&OsStr]) -> Result<()> {
    run_checked(host, "sudo", args).map(drop)
}
