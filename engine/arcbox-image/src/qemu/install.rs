//! Extracting, checking and publishing a QEMU archive under the runtime root.
//!
//! The root holds one directory per installed version, the lock every
//! installer takes, and the download in progress:
//!
//! ```text
//! <root>/.lock
//! <root>/.downloads/<sha256>.part
//! <root>/<version>/{bin,lib,licenses,BUILD-INFO.json,.archive-sha256}
//! ```

use std::fs::File;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::Digest as _;

use super::signature::Verify;
use super::{QEMU_EXECUTABLE, QemuArtifact};
use crate::error::{ImageError, Result};
use crate::remote_image::StagingGuard;

const LOCK: &str = ".lock";
/// Written last into a staged build: an install without it is incomplete.
const MARKER: &str = ".archive-sha256";

/// Takes the root's exclusive lock, blocking until it is free.
pub(super) fn lock(root: &Path) -> Result<File> {
    let file = open_lock(root)?;
    file.lock()?;
    Ok(file)
}

fn open_lock(root: &Path) -> Result<File> {
    std::fs::create_dir_all(root)?;
    Ok(File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(LOCK))?)
}

/// The executable of `artifact`'s installed build, if its marker matches.
pub(super) fn installed(root: &Path, artifact: &QemuArtifact) -> Option<PathBuf> {
    let directory = root.join(&artifact.version);
    let marker = std::fs::read_to_string(directory.join(MARKER)).ok()?;
    let executable = directory.join(QEMU_EXECUTABLE);
    (marker == artifact.sha256 && executable.is_file()).then_some(executable)
}

/// Fails unless `archive` hashes to the pinned SHA-256.
pub(super) fn check_sha256(archive: &Path, artifact: &QemuArtifact) -> Result<()> {
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut File::open(archive)?, &mut hasher)?;
    let actual = format!("{:x}", hasher.finalize());
    if actual == artifact.sha256 {
        return Ok(());
    }
    Err(arcbox_asset::AssetError::ChecksumMismatch {
        name: artifact.archive_name(),
        expected: artifact.sha256.clone(),
        actual,
    }
    .into())
}

/// Extracts `archive` into a staging directory, checks it, and moves it to
/// `<root>/<version>`, replacing an older install of that version. The
/// caller holds the lock and has checked the archive's SHA-256. Returns the
/// executable.
pub(super) fn install(
    root: &Path,
    artifact: &QemuArtifact,
    archive: &Path,
    verifier: &dyn Verify,
) -> Result<PathBuf> {
    let staging = root.join(unique(".staging"));
    std::fs::create_dir_all(&staging)?;
    let mut guard = StagingGuard::new(staging.clone());
    extract(archive, &staging)?;
    check_layout(&staging, artifact)?;
    for (path, executable) in binaries(&staging)? {
        let requirement = artifact.signature.requirement(executable);
        verifier
            .verify(&path, requirement.as_deref())
            .map_err(|reason| {
                let name = path.strip_prefix(&staging).unwrap_or(&path).display();
                ImageError::QemuBuild(format!("{name} fails its code signature check: {reason}"))
            })?;
    }
    std::fs::write(staging.join(MARKER), &artifact.sha256)?;

    let destination = root.join(&artifact.version);
    if destination.exists() {
        let retired = root.join(unique(".retired"));
        std::fs::rename(&destination, &retired)?;
        let _ = std::fs::remove_dir_all(retired);
    }
    std::fs::rename(&staging, &destination)?;
    guard.disarm();
    Ok(destination.join(QEMU_EXECUTABLE))
}

fn unique(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{seq}", std::process::id())
}

/// Unpacks regular files and directories only, all below `into`.
fn extract(archive: &Path, into: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(BufReader::new(File::open(archive)?));
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        let inside = path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !(kind.is_file() || kind.is_dir()) || !inside {
            return Err(ImageError::QemuBuild(format!(
                "archive entry {} is not a file or directory inside the archive",
                path.display()
            )));
        }
        entry.unpack_in(into)?;
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct BuildInfo {
    version: String,
    arch: String,
    executable: String,
}

/// The archive must describe the pinned build and ship its licenses.
fn check_layout(staging: &Path, artifact: &QemuArtifact) -> Result<()> {
    let info: BuildInfo = serde_json::from_slice(&std::fs::read(staging.join("BUILD-INFO.json"))?)
        .map_err(|error| ImageError::QemuBuild(format!("BUILD-INFO.json: {error}")))?;
    if (
        info.version.as_str(),
        info.arch.as_str(),
        info.executable.as_str(),
    ) != (
        artifact.version.as_str(),
        artifact.arch.as_str(),
        QEMU_EXECUTABLE,
    ) {
        return Err(ImageError::QemuBuild(format!(
            "the archive is {} {} with executable {}, expected {} {} with {QEMU_EXECUTABLE}",
            info.version, info.arch, info.executable, artifact.version, artifact.arch
        )));
    }
    let executable = std::fs::metadata(staging.join(QEMU_EXECUTABLE))?;
    if !executable.is_file() || executable.permissions().mode() & 0o111 == 0 {
        return Err(ImageError::QemuBuild(format!(
            "{QEMU_EXECUTABLE} is not an executable file"
        )));
    }
    if std::fs::read_dir(staging.join("licenses"))?
        .next()
        .is_none()
    {
        return Err(ImageError::QemuBuild(
            "the archive ships no licenses".into(),
        ));
    }
    Ok(())
}

/// Every regular file under `bin/` and `lib/`, flagged when it is the
/// executable. All of them must be signed.
fn binaries(staging: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut found = Vec::new();
    let mut pending = vec![staging.join("bin"), staging.join("lib")];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let executable = path == staging.join(QEMU_EXECUTABLE);
                found.push((path, executable));
            }
        }
    }
    Ok(found)
}
