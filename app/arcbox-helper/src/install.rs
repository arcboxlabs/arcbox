//! Stop the loaded helper before the installer replaces its executable.

mod processes;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use arcbox_constants::paths::{labels, privileged};
use arcbox_helper::HelperError;

/// Stops the loaded helper and waits for every installed helper process to exit.
pub fn stop_helper() -> Result<(), HelperError> {
    // SAFETY: geteuid has no arguments or failure case.
    if unsafe { libc::geteuid() } != 0 {
        return Err(HelperError::validation(
            "helper installation preparation requires root",
        ));
    }
    let executable = installed_executable()?;
    let target = format!("system/{}", labels::HELPER);
    let output = Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output()
        .map_err(|error| HelperError::io("bootout", &target, error))?;
    if !output.status.success() && output.status.code() != Some(libc::ESRCH) {
        return Err(HelperError::other(format!(
            "launchctl bootout {target} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let output = Command::new("/bin/launchctl")
        .arg("list")
        .output()
        .map_err(|error| HelperError::io("list services", &target, error))?;
    if !output.status.success() {
        return Err(HelperError::other(format!(
            "launchctl list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let listing = std::str::from_utf8(&output.stdout)
        .map_err(|error| HelperError::other(error.to_string()))?;
    if service_present(listing)? {
        return Err(HelperError::other(
            "helper service remains loaded after bootout",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(50);
    loop {
        let pids = processes::helper_pids(&executable)
            .map_err(|error| HelperError::io("inspect processes", &executable, error))?;
        if pids.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(HelperError::other(format!(
                "helper processes did not exit after bootout: {pids:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn installed_executable() -> Result<PathBuf, HelperError> {
    let path = Path::new(privileged::HELPER_BINARY);
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path.to_owned()),
        Err(error) => return Err(HelperError::io("inspect executable", path, error)),
    };
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(HelperError::other(format!(
            "{} must be a root-owned regular file without group or other write permission",
            path.display()
        )));
    }
    fs::canonicalize(path).map_err(|error| HelperError::io("resolve executable", path, error))
}

fn service_present(listing: &str) -> Result<bool, HelperError> {
    let mut lines = listing.lines();
    if !lines
        .next()
        .is_some_and(|line| line.split_whitespace().eq(["PID", "Status", "Label"]))
    {
        return Err(HelperError::other("unexpected launchctl list header"));
    }
    for line in lines {
        // Labels may contain whitespace; only the first two tabs delimit columns.
        let label = line
            .splitn(3, '\t')
            .nth(2)
            .ok_or_else(|| HelperError::other("unexpected launchctl list row"))?;
        if label == labels::HELPER {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_listing_matches_exact_labels_and_rejects_unknown_shapes() {
        assert!(!service_present("PID\tStatus\tLabel\n10\t0\tanother helper\n").unwrap());
        assert!(
            service_present(&format!("PID\tStatus\tLabel\n-\t0\t{}\n", labels::HELPER)).unwrap()
        );
        assert!(
            !service_present(&format!(
                "PID\tStatus\tLabel\n10\t0\t{}.other\n",
                labels::HELPER
            ))
            .unwrap()
        );
        assert!(
            !service_present(&format!(
                "PID\tStatus\tLabel\n10\t0\t{}\tother\n",
                labels::HELPER
            ))
            .unwrap()
        );
        for listing in ["", "PID Label\n", "PID Status Label\nmalformed\n"] {
            assert!(service_present(listing).is_err());
        }
    }
}
