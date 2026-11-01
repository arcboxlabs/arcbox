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
//! A clone is made under a temporary name, made private and durable, and
//! only then renamed into place without replacing anything. A crash at any
//! point before that rename leaves only a `.clone-*` temporary, which
//! [`CloneStore::reconcile_stale`] deletes at startup; a crash after it
//! leaves a disk no journal names yet, which the same sweep deletes as an
//! unowned overlay.
//!
//! `clonefile` needs the template and the data directory on the same APFS
//! volume. Anywhere else, and off macOS, setup fails with an ordinary I/O
//! error, which the runtime reads as "no CoW here" and answers with a full
//! copy of the rootfs.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::{debug, info, warn};

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
    /// The file a failed setup could not delete, by owner. The owner cannot set
    /// up again until [`Self::cleanup_setup_orphan`] has removed them, the
    /// same rule the dm path applies to its half-built devices.
    orphans: Mutex<HashMap<String, PathBuf>>,
}

impl CloneStore {
    pub(super) fn new(cow_dir: PathBuf) -> Self {
        Self {
            cow_dir,
            orphans: Mutex::new(HashMap::new()),
        }
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
        self.refuse_orphaned(owner)?;
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
        self.refuse_orphaned(owner)?;
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

    /// Confirm a running sandbox's disk is still there. Nothing else to
    /// re-register: a clone holds no shared resource.
    pub(super) fn adopt(&self, handle: &CowHandle) -> Result<()> {
        let disk = self.owned_disk(handle)?;
        require_disk(disk, "sandbox disk")?;
        info!(disk = %disk.display(), "adopted a live sandbox's disk");
        Ok(())
    }

    /// Delete every half-made clone and every disk whose owner is in
    /// neither set. Same contract as the dm path's sweep, and like it, runs
    /// at startup before any setup: a `.clone-*` file is only ever a setup in
    /// flight or one a crash interrupted.
    pub(super) fn reconcile_stale(
        &self,
        keep_cow_files: &HashSet<String>,
        live_devices: &HashSet<String>,
    ) -> Result<()> {
        let mut removed = false;
        for entry in std::fs::read_dir(&self.cow_dir)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(CLONE_TEMP_PREFIX) {
                debug!(file = %path.display(), "removing half-made clone");
            } else if let Some(rest) = name.strip_prefix(COW_FILE_PREFIX) {
                let owner = rest.strip_suffix(COW_FILE_SUFFIX).unwrap_or(rest);
                if keep_cow_files.contains(owner) || live_devices.contains(owner) {
                    debug!(file = %path.display(), "keeping retained sandbox disk");
                    continue;
                }
                debug!(file = %path.display(), "removing stale sandbox disk");
            } else {
                continue;
            }
            std::fs::remove_file(&path)?;
            removed = true;
        }
        if removed {
            std::fs::File::open(&self.cow_dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Retry the deletes a failed setup left undone.
    pub(super) fn cleanup_setup_orphan(&self, owner: &str) -> Result<()> {
        let Some(path) = self.orphans.lock().unwrap().get(owner).cloned() else {
            return Ok(());
        };
        remove_file_durable(&path)?;
        self.orphans.lock().unwrap().remove(owner);
        Ok(())
    }

    fn refuse_orphaned(&self, owner: &str) -> Result<()> {
        if self.orphans.lock().unwrap().contains_key(owner) {
            return Err(SnapshotError::Unavailable(format!(
                "sandbox {owner} still owns files from an incomplete clone"
            )));
        }
        Ok(())
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

    /// Delete what a failed setup made, and turn what it could not delete
    /// into an orphan that blocks the owner until it is cleaned up.
    fn rollback(&self, owner: &str, failure: SetupFailure) -> SnapshotError {
        let Some(made) = failure.made else {
            return failure.error.into();
        };
        let Err(cleanup) = remove_file_durable(&made) else {
            return failure.error.into();
        };
        warn!(owner, file = %made.display(), "clone rollback incomplete");
        let message = format!(
            "{}; resource rollback is incomplete: remove {}: {cleanup}",
            failure.error,
            made.display()
        );
        self.orphans.lock().unwrap().insert(owner.to_owned(), made);
        SnapshotError::Unavailable(message)
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::snapshot_cow::{CowBackend, CowManager, CowOptions};

    const ROOTFS: &[u8] = b"template rootfs blocks";

    /// A clonefile manager under `{root}/data` and a template under
    /// `{root}/templates`, on the same volume as `clonefile` requires.
    fn fixture() -> (tempfile::TempDir, CowManager, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let templates = root.path().join("templates");
        std::fs::create_dir(&templates).unwrap();
        let template = templates.join("rootfs.ext4");
        std::fs::write(&template, ROOTFS).unwrap();
        // World-readable, so the private mode of the clone is the clone's
        // own doing and not inherited.
        std::fs::set_permissions(&template, std::fs::Permissions::from_mode(0o644)).unwrap();
        let manager = CowManager::new(CowOptions {
            backend: CowBackend::Clonefile,
            ..CowOptions::new(root.path().join("data"))
        })
        .unwrap();
        (root, manager, template)
    }

    fn cow_dir(root: &tempfile::TempDir) -> PathBuf {
        root.path().join("data").join("cow")
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn template_str(template: &Path) -> &str {
        template.to_str().unwrap()
    }

    #[test]
    fn the_host_backend_needs_no_device_mapper_state() {
        let (root, _manager, _) = fixture();
        assert!(
            entries(&cow_dir(&root)).is_empty(),
            "the clonefile backend created dm bookkeeping"
        );
    }

    /// Writes stay on the side that made them, which is the whole point:
    /// two sandboxes from one template, and the template itself, never see
    /// each other's blocks.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn setup_gives_each_sandbox_a_private_copy_on_write_disk() {
        let (root, manager, template) = fixture();

        let first = manager.setup("one", template_str(&template)).await.unwrap();
        let second = manager.setup("two", template_str(&template)).await.unwrap();

        let disk = cow_dir(&root).join("arcbox-cow-one.img");
        assert_eq!(first.cow_file, disk);
        assert_eq!(first.dm_device, disk.to_str().unwrap());
        assert_eq!(first.dm_name, "arcbox-snap-one");
        assert!(first.cow_loop.is_empty());
        assert_eq!(first.template_path, template);
        assert_eq!(std::fs::read(&first.cow_file).unwrap(), ROOTFS);
        let mode = std::fs::metadata(&first.cow_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        std::fs::write(&first.cow_file, b"written by sandbox one").unwrap();
        assert_eq!(std::fs::read(&template).unwrap(), ROOTFS);
        assert_eq!(std::fs::read(&second.cow_file).unwrap(), ROOTFS);
        assert_eq!(
            entries(&cow_dir(&root)),
            ["arcbox-cow-one.img", "arcbox-cow-two.img"],
            "setup left a temporary behind"
        );
    }

    /// An existing disk is some sandbox's state, and setup is not Remove.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn setup_never_replaces_an_existing_disk() {
        let (root, manager, template) = fixture();
        let disk = cow_dir(&root).join("arcbox-cow-box.img");
        std::fs::write(&disk, b"a paused sandbox's disk").unwrap();

        let error = manager
            .setup("box", template_str(&template))
            .await
            .unwrap_err();

        assert!(
            matches!(&error, SnapshotError::Common(arcbox_error::CommonError::Io(io)) if io.kind() == io::ErrorKind::AlreadyExists),
            "{error}"
        );
        assert_eq!(std::fs::read(&disk).unwrap(), b"a paused sandbox's disk");
        assert_eq!(entries(&cow_dir(&root)), ["arcbox-cow-box.img"]);
    }

    /// Not `Unavailable`: the runtime reads any other setup error as "no
    /// CoW here" and stages a full copy instead, which is the right answer
    /// for a template on another volume.
    #[tokio::test]
    async fn a_template_that_cannot_be_cloned_fails_in_a_way_the_caller_falls_back_from() {
        let (root, manager, _) = fixture();
        let missing = root.path().join("templates").join("absent.ext4");

        let error = manager
            .setup("box", template_str(&missing))
            .await
            .unwrap_err();

        assert!(!matches!(error, SnapshotError::Unavailable(_)), "{error}");
        assert!(entries(&cow_dir(&root)).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn teardown_deletes_only_the_disk_and_is_idempotent() {
        let (_root, manager, template) = fixture();
        let handle = manager.setup("box", template_str(&template)).await.unwrap();

        manager.teardown_checked(&handle).await.unwrap();
        manager.teardown_checked(&handle).await.unwrap();

        assert!(!handle.cow_file.exists());
        assert_eq!(std::fs::read(&template).unwrap(), ROOTFS);
    }

    /// A journal hands the handle back, and teardown deletes what it names;
    /// a path outside this store's directory is refused, not unlinked.
    #[tokio::test]
    async fn teardown_refuses_a_handle_naming_a_foreign_file() {
        let (root, manager, template) = fixture();
        let foreign = root.path().join("arcbox-cow-box.img");
        std::fs::write(&foreign, b"not ours").unwrap();
        let handle = CowHandle {
            dm_name: "arcbox-snap-box".into(),
            dm_device: foreign.to_string_lossy().into_owned(),
            cow_loop: String::new(),
            cow_file: foreign.clone(),
            template_path: template,
        };

        let error = manager.teardown_checked(&handle).await.unwrap_err();

        assert!(
            matches!(error, SnapshotError::FailedPrecondition(_)),
            "{error}"
        );
        assert!(foreign.exists());
    }

    /// Pause keeps the disk with every write; resume hands back the same
    /// file, even after the template it was cloned from has gone.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_paused_disk_reattaches_with_its_writes_and_without_its_template() {
        let (_root, manager, template) = fixture();
        let handle = manager.setup("box", template_str(&template)).await.unwrap();
        std::fs::write(&handle.cow_file, b"guest writes").unwrap();

        manager.detach_keep_cow(&handle).await.unwrap();
        std::fs::remove_file(&template).unwrap();
        let resumed = manager
            .reattach("box", template_str(&template))
            .await
            .unwrap();

        assert_eq!(resumed.cow_file, handle.cow_file);
        assert_eq!(std::fs::read(&resumed.cow_file).unwrap(), b"guest writes");

        manager.remove_preserved_cow("box").await.unwrap();
        manager.remove_preserved_cow("box").await.unwrap();
        assert!(!resumed.cow_file.exists());
    }

    #[tokio::test]
    async fn reattach_and_adopt_refuse_a_missing_disk() {
        let (root, manager, template) = fixture();

        let reattach = manager
            .reattach("box", template_str(&template))
            .await
            .unwrap_err();
        let adopt = manager
            .adopt(&CowHandle {
                dm_name: "arcbox-snap-box".into(),
                dm_device: String::new(),
                cow_loop: String::new(),
                cow_file: cow_dir(&root).join("arcbox-cow-box.img"),
                template_path: template,
            })
            .unwrap_err();

        for error in [reattach, adopt] {
            assert!(
                matches!(&error, SnapshotError::FailedPrecondition(m) if m.contains("arcbox-cow-box.img")),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn remove_preserved_refuses_an_owner_that_is_a_path() {
        let (root, manager, _) = fixture();
        let outside = cow_dir(&root).join("arcbox-cow-..").join("victim.img");

        assert!(manager.remove_preserved_cow("../victim").await.is_err());
        assert!(!outside.exists());
    }

    /// Crash recovery: a crash during setup leaves a `.clone-*` temporary
    /// (before the rename) or a disk no journal names (after it). Both go;
    /// retained and live disks, and files that are not this store's, stay.
    #[test]
    fn the_startup_sweep_removes_half_made_clones_and_unowned_disks() {
        let (root, manager, _) = fixture();
        let dir = cow_dir(&root);
        for name in [
            ".clone-crashed-0123456789abcdef.tmp",
            "arcbox-cow-unjournaled.img",
            "arcbox-cow-paused.img",
            "arcbox-cow-running.img",
            "unrelated.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        manager
            .reconcile_stale(
                &HashSet::from(["paused".to_owned()]),
                &HashSet::from(["running".to_owned()]),
            )
            .unwrap();

        assert_eq!(
            entries(&dir),
            [
                "arcbox-cow-paused.img",
                "arcbox-cow-running.img",
                "unrelated.txt"
            ]
        );
    }

    /// A setup whose rollback could not delete what it made holds its owner
    /// until the cleanup retry succeeds, as on the dm path.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_incomplete_rollback_blocks_the_owner_until_cleanup() {
        let (root, manager, template) = fixture();
        let stranded = cow_dir(&root).join(".clone-box-stranded.tmp");
        std::fs::write(&stranded, b"x").unwrap();
        let clones = manager.clones.as_ref().unwrap();
        clones
            .orphans
            .lock()
            .unwrap()
            .insert("box".into(), stranded.clone());

        let refused = manager
            .setup("box", template_str(&template))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, SnapshotError::Unavailable(_)),
            "{refused}"
        );

        manager.cleanup_setup_orphan("box").await.unwrap();
        assert!(!stranded.exists());
        manager.setup("box", template_str(&template)).await.unwrap();
    }

    /// The rollback itself: a publish that fails leaves nothing behind, and
    /// a delete that fails is recorded rather than forgotten.
    #[test]
    fn rollback_records_only_what_it_could_not_delete() {
        let root = tempfile::tempdir().unwrap();
        let store = CloneStore::new(root.path().to_path_buf());
        let removable = root.path().join(".clone-box-a.tmp");
        std::fs::write(&removable, b"x").unwrap();
        // A non-empty directory: `remove_file` refuses it on every platform.
        let stuck = root.path().join(".clone-box-b.tmp");
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("inner"), b"x").unwrap();

        let clean = store.rollback(
            "clean",
            SetupFailure {
                error: io::Error::other("publish failed"),
                made: Some(removable.clone()),
            },
        );
        let dirty = store.rollback(
            "box",
            SetupFailure {
                error: io::Error::other("publish failed"),
                made: Some(stuck.clone()),
            },
        );

        assert!(!matches!(clean, SnapshotError::Unavailable(_)), "{clean}");
        assert!(!removable.exists());
        assert!(matches!(dirty, SnapshotError::Unavailable(_)), "{dirty}");
        assert_eq!(store.orphans.lock().unwrap().get("box"), Some(&stuck));
        assert!(!store.orphans.lock().unwrap().contains_key("clean"));
    }
}
