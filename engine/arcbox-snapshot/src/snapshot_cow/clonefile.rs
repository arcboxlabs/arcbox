//! The host backend of [`CowManager`]: every sandbox disk is an APFS
//! `clonefile(2)` copy of its template.
//!
//! A clone shares the template's extents until either side writes, so it
//! costs what dm-snapshot's sparse overlay costs, but it is an ordinary file
//! the VMM opens directly: there is no loop device, no device-mapper table
//! and no template refcount. Nothing pins the template either — a clone stays
//! valid when the template is rebuilt or deleted — so teardown is one unlink
//! and pause is an `fsync`.
//!
//! The [`CowHandle`] keeps its dm-mode field names: `dm_device` and
//! `cow_file` are both the clone (the disk the VM opens), `cow_loop` is
//! empty, and `dm_name` is still `arcbox-snap-{owner}`, so a composer reads
//! the owner back out of either name exactly as it does on the dm path.
//!
//! `clonefile` needs the template and the data directory on the same APFS
//! volume. Anywhere else, and off macOS, setup fails with an ordinary I/O
//! error, which the runtime reads as "no CoW here" and answers with a full
//! copy of the rootfs.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::persistence::remove_file_durable;
use super::{
    COW_FILE_PREFIX, COW_FILE_SUFFIX, CowHandle, DM_NAME_PREFIX, run_blocking,
    validate_dm_name_suffix,
};
use crate::error::{Result, SnapshotError};

/// File-name prefix of a clone that is still being made.
const CLONE_TEMP_PREFIX: &str = ".clone-";

/// Per-sandbox clones of template images under one directory.
pub(super) struct CloneStore {
    cow_dir: PathBuf,
}

impl CloneStore {
    pub(super) fn new(cow_dir: PathBuf) -> Self {
        Self { cow_dir }
    }

    fn disk_path(&self, owner: &str) -> PathBuf {
        self.cow_dir
            .join(format!("{COW_FILE_PREFIX}{owner}{COW_FILE_SUFFIX}"))
    }

    fn handle(&self, owner: &str, template: &Path) -> CowHandle {
        let disk = self.disk_path(owner);
        CowHandle {
            dm_name: format!("{DM_NAME_PREFIX}{owner}"),
            dm_device: disk.to_string_lossy().into_owned(),
            cow_loop: String::new(),
            cow_file: disk,
            template_path: template.to_path_buf(),
        }
    }

    /// Clone `template` into a new disk for `owner`.
    ///
    /// Refuses to replace an existing disk: that file is some sandbox's
    /// state, a paused one's perhaps, and only Remove may delete it.
    pub(super) async fn setup(&self, owner: &str, template: &Path) -> Result<CowHandle> {
        let disk = self.disk_path(owner);
        let temporary = self.cow_dir.join(format!(
            "{CLONE_TEMP_PREFIX}{owner}-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let source = template.to_path_buf();
        let (staged, published) = (temporary.clone(), disk.clone());
        let cow_dir = self.cow_dir.clone();
        let made =
            run_blocking(move || Ok(make_disk(&source, &staged, &published, &cow_dir))).await?;
        if let Err(failure) = made {
            return Err(self.rollback(owner, failure));
        }
        info!(owner, disk = %disk.display(), template = %template.display(), "cloned sandbox disk");
        Ok(self.handle(owner, template))
    }

    /// The handle of a paused sandbox's retained disk.
    ///
    /// The clone is self-contained, so the template is only recorded, never
    /// consulted: a template rebuilt since the pause does not invalidate it.
    pub(super) fn reattach(&self, owner: &str, template: &Path) -> Result<CowHandle> {
        let disk = self.disk_path(owner);
        require_disk(&disk, "preserved sandbox disk")?;
        Ok(self.handle(owner, template))
    }

    /// Delete the sandbox's disk. Idempotent.
    pub(super) fn teardown(&self, handle: &CowHandle) -> Result<()> {
        let disk = self.owned_disk(handle)?;
        remove_file_durable(disk)?;
        info!(disk = %disk.display(), "sandbox disk removed");
        Ok(())
    }

    /// Keep the disk for a later [`Self::reattach`], flushed to stable
    /// storage: the checkpoint written next to it is durable, and a disk
    /// that loses its last writes to a power cut no longer matches it.
    pub(super) fn detach_keep(&self, handle: &CowHandle) -> Result<()> {
        let disk = self.owned_disk(handle)?;
        require_disk(disk, "sandbox disk")?;
        // `sync_all` is `F_FULLFSYNC` on Apple targets, which is what makes
        // the drive's own cache part of "durable".
        std::fs::File::open(disk)?.sync_all()?;
        info!(disk = %disk.display(), "sandbox disk retained");
        Ok(())
    }

    /// Delete the disk a pause left behind. Idempotent.
    pub(super) fn remove_preserved(&self, owner: &str) -> Result<()> {
        // The owner becomes a path component of a file this deletes.
        validate_dm_name_suffix(owner)?;
        remove_file_durable(&self.disk_path(owner))
    }

    /// The handle's disk, if it is one this store would have made. A handle
    /// comes back from a journal, and this store deletes what it names.
    fn owned_disk<'h>(&self, handle: &'h CowHandle) -> Result<&'h Path> {
        let named_here = handle.cow_file.parent() == Some(self.cow_dir.as_path())
            && handle
                .cow_file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(COW_FILE_PREFIX));
        if named_here {
            Ok(&handle.cow_file)
        } else {
            Err(SnapshotError::FailedPrecondition(format!(
                "{} is not a sandbox disk under {}",
                handle.cow_file.display(),
                self.cow_dir.display()
            )))
        }
    }

    /// Delete what a failed setup made.
    fn rollback(&self, owner: &str, failure: SetupFailure) -> SnapshotError {
        let Some(made) = failure.made else {
            return failure.error.into();
        };
        let Err(cleanup) = remove_file_durable(&made) else {
            return failure.error.into();
        };
        warn!(owner, file = %made.display(), "clone rollback incomplete");
        SnapshotError::Unavailable(format!(
            "{}; resource rollback is incomplete: remove {}: {cleanup}",
            failure.error,
            made.display()
        ))
    }
}

/// A setup step's error and the file that setup had made by then.
struct SetupFailure {
    error: io::Error,
    made: Option<PathBuf>,
}

/// Clone `template` to `temporary`, make it private and durable, then move
/// it to `disk` without replacing anything there.
fn make_disk(
    template: &Path,
    temporary: &Path,
    disk: &Path,
    cow_dir: &Path,
) -> std::result::Result<(), SetupFailure> {
    let step = |result: io::Result<()>, what: String, made: Option<&Path>| {
        result.map_err(|error| SetupFailure {
            error: io::Error::new(error.kind(), format!("{what}: {error}")),
            made: made.map(Path::to_path_buf),
        })
    };
    let (from, to) = (template.display(), temporary.display());
    step(
        clone_file(template, temporary),
        format!("clone {from} to {to}"),
        None,
    )?;
    step(
        secure_and_sync(temporary),
        format!("secure {to}"),
        Some(temporary),
    )?;
    let publish = rename_exclusive(temporary, disk);
    step(
        publish,
        format!("publish {}", disk.display()),
        Some(temporary),
    )?;
    let sync = std::fs::File::open(cow_dir).and_then(|directory| directory.sync_all());
    step(sync, format!("sync {}", cow_dir.display()), Some(disk))
}

/// The clone inherits the template's mode; a sandbox disk is private.
fn secure_and_sync(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.sync_all()
}

fn require_disk(disk: &Path, what: &str) -> Result<()> {
    match std::fs::symlink_metadata(disk) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(SnapshotError::FailedPrecondition(format!(
            "{what} {} is not a regular file",
            disk.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(
            SnapshotError::FailedPrecondition(format!("{what} {} is gone", disk.display())),
        ),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// `clonefile(2)` without the source's owner: the clone belongs to this
/// process. A symlinked template is followed, the way every other reader of
/// the template path follows it.
#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    /// `<sys/clonefile.h>`; the `libc` crate does not carry the flags.
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    let (source, destination) = (c_path(source)?, c_path(destination)?);
    // SAFETY: both pointers come from live `CString`s, NUL-terminated and
    // valid for the duration of the call, which does not retain them.
    let status =
        unsafe { nix::libc::clonefile(source.as_ptr(), destination.as_ptr(), CLONE_NOOWNERCOPY) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_file(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clonefile copy-on-write is only available on macOS",
    ))
}

/// Never reached: [`clone_file`] has already failed.
#[cfg(not(target_os = "macos"))]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    clone_file(from, to)
}

/// `rename` that fails with `EEXIST` instead of replacing `to`.
#[cfg(target_os = "macos")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both pointers come from live `CString`s, NUL-terminated and
    // valid for the duration of the call, which does not retain them.
    let status =
        unsafe { nix::libc::renamex_np(from.as_ptr(), to.as_ptr(), nix::libc::RENAME_EXCL) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

