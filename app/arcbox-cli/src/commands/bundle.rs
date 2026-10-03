//! Locating ArcBox's other binaries relative to the running `abctl`.
//!
//! `abctl` is usually reached through a symlink: `/opt/homebrew/bin/abctl`
//! from the Homebrew cask, `~/.arcbox/bin/abctl` from `abctl setup install`.
//! Every lookup therefore starts from the resolved executable, not from the
//! path it was invoked by (#644). Inside the Desktop app the layout is fixed:
//!
//! ```text
//! ArcBox.app/Contents/
//! ├── MacOS/bin/abctl
//! ├── MacOS/xbin/docker…                              CLI tools
//! └── Frameworks/<label>.app/Contents/MacOS/<label>   the daemon
//! ```
//!
//! The privileged `/usr/local/bin/*` symlinks into `xbin/` are managed by
//! `arcbox-helper`; the user-space `~/.arcbox/bin/*` symlinks by
//! `setup::install`. Both take the `xbin/` directory from here.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::labels;

/// File name of the daemon binary outside the Desktop bundle.
const DAEMON_BINARY: &str = "arcbox-daemon";

/// The running `abctl`, with every symlink resolved.
pub fn current_executable() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not determine current executable")?;
    std::fs::canonicalize(&exe).with_context(|| format!("could not resolve {}", exe.display()))
}

/// Detects the `xbin/` directory inside the current app bundle.
///
/// Returns `None` outside an app bundle.
pub fn detect_bundle_xbin() -> Option<PathBuf> {
    let exe = current_executable().ok()?;
    let xbin = bundle_contents(&exe)?.join("MacOS/xbin");
    xbin.is_dir().then_some(xbin)
}

/// Finds the daemon binary to launch or register.
///
/// Looks next to `abctl`, inside the Desktop bundle `abctl` runs from, at
/// `<data_dir>/bin/arcbox-daemon`, then on `PATH`.
pub fn locate_daemon(data_dir: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not determine current executable")?;
    locate_daemon_from(&exe, data_dir, std::env::var_os("PATH").as_deref())
}

fn locate_daemon_from(
    invoked: &Path,
    data_dir: &Path,
    path_var: Option<&OsStr>,
) -> Result<PathBuf> {
    let exe = std::fs::canonicalize(invoked)
        .with_context(|| format!("could not resolve {}", invoked.display()))?;
    let candidates = daemon_candidates(&exe, data_dir, path_var);
    if let Some(found) = candidates.iter().find(|candidate| candidate.is_file()) {
        return Ok(found.clone());
    }
    bail!(
        "could not find {DAEMON_BINARY}: not next to {}, not in its app bundle, not at {}, and not on PATH.\n\
         Install ArcBox.app (brew install --cask arcbox), or put {DAEMON_BINARY} on PATH.",
        exe.display(),
        data_dir.join("bin").join(DAEMON_BINARY).display(),
    )
}

/// Every place the daemon may live, most specific first.
fn daemon_candidates(exe: &Path, data_dir: &Path, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join(DAEMON_BINARY));
    }
    if let Some(contents) = bundle_contents(exe) {
        for label in [labels::DAEMON, labels::DEVELOPMENT_DAEMON] {
            candidates.push(
                contents
                    .join("Frameworks")
                    .join(format!("{label}.app"))
                    .join("Contents/MacOS")
                    .join(label),
            );
            // Bundles built before the daemon moved into its own app.
            candidates.push(contents.join("Helpers").join(label));
        }
    }
    candidates.push(data_dir.join("bin").join(DAEMON_BINARY));
    if let Some(path) = path_var {
        candidates.extend(std::env::split_paths(path).map(|dir| dir.join(DAEMON_BINARY)));
    }
    candidates
}

/// The main bundle's `Contents/` when `exe` sits at `Contents/MacOS/bin/`.
fn bundle_contents(exe: &Path) -> Option<&Path> {
    let contents = exe.parent()?.parent()?.parent()?;
    (contents.file_name() == Some(OsStr::new("Contents"))).then_some(contents)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::locate_daemon_from;

    fn touch(path: &Path) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\n").unwrap();
        path.to_path_buf()
    }

    fn same_file(a: &Path, b: &Path) {
        assert_eq!(
            fs::canonicalize(a).unwrap(),
            fs::canonicalize(b).unwrap(),
            "{} should be {}",
            a.display(),
            b.display()
        );
    }

    /// The Homebrew cask links `abctl` into `/opt/homebrew/bin`; the daemon
    /// is the app inside `Contents/Frameworks`, two levels away from the
    /// link's target (#644).
    #[test]
    fn the_bundled_daemon_is_found_through_a_symlinked_abctl() {
        let root = tempfile::tempdir().unwrap();
        let contents = root.path().join("Applications/ArcBox.app/Contents");
        let abctl = touch(&contents.join("MacOS/bin/abctl"));
        let daemon = touch(&contents.join(
            "Frameworks/com.arcboxlabs.desktop.daemon.app/Contents/MacOS/com.arcboxlabs.desktop.daemon",
        ));
        let link = root.path().join("homebrew/bin/abctl");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&abctl, &link).unwrap();
        let data_dir = root.path().join("home/.arcbox");

        let found = locate_daemon_from(&link, &data_dir, None).unwrap();
        same_file(&found, &daemon);

        // The legacy bundle layout kept the daemon under Contents/Helpers.
        fs::remove_file(&daemon).unwrap();
        let legacy = touch(&contents.join("Helpers/com.arcboxlabs.desktop.daemon"));
        same_file(
            &locate_daemon_from(&link, &data_dir, None).unwrap(),
            &legacy,
        );
    }

    #[test]
    fn a_sibling_daemon_wins_and_the_data_dir_and_path_are_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let abctl = touch(&root.path().join("release/abctl"));
        let data_dir = root.path().join("home/.arcbox");
        let on_path = root.path().join("path-dir");
        let path_var = std::env::join_paths([&on_path]).unwrap();

        let err = locate_daemon_from(&abctl, &data_dir, Some(&path_var)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("arcbox-daemon"), "{message}");
        assert!(message.contains("brew install --cask arcbox"), "{message}");

        let from_path = touch(&on_path.join("arcbox-daemon"));
        same_file(
            &locate_daemon_from(&abctl, &data_dir, Some(&path_var)).unwrap(),
            &from_path,
        );

        let from_data_dir = touch(&data_dir.join("bin/arcbox-daemon"));
        same_file(
            &locate_daemon_from(&abctl, &data_dir, Some(&path_var)).unwrap(),
            &from_data_dir,
        );

        let sibling = touch(&root.path().join("release/arcbox-daemon"));
        same_file(
            &locate_daemon_from(&abctl, &data_dir, Some(&path_var)).unwrap(),
            &sibling,
        );
    }
}
