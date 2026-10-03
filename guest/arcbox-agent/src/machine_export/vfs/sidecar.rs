//! The Mac's AppleDouble sidecars, kept out of the machine.
//!
//! The macOS NFS client cannot store extended attributes on an NFSv3
//! server, so it writes them as a `._<name>` sibling in AppleDouble format,
//! and recent macOS stamps `com.apple.provenance` on every file a process
//! of a downloaded app creates, so every file the Mac writes into a machine
//! comes with one. Left on disk they litter it: `ls -a` shows them, and git
//! takes a `._pack-*.idx` for a pack index and fails. Nothing in Linux wants
//! them.
//!
//! So a `._` file the Mac creates where none exists on disk never reaches
//! the filesystem. It lives in [`Sidecars`], answered to lookups as written
//! but never listed — the Mac sees its extended attributes, not files, as
//! on a native volume, and git's pack scan on the Mac never meets a
//! `._pack-*.idx` either — until the Mac removes it, the table evicts it,
//! or the machine stops. A `._` file that does exist on disk — made inside
//! the machine — is a plain file, listed and served from disk; one
//! appearing on disk under a sidecar's name takes its place. The Mac's
//! metadata on a machine's files thus lasts as long as the machine runs: a
//! dropped sidecar reads as "no attribute", and the Mac writes it again on
//! the next `setxattr`.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::{MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use nfs3_server::nfs3_types::nfs3::{
    fattr3, ftype3, nfsstat3, nfstime3, sattr3, set_mode3, set_size3, specdata3,
};
use nfs3_server::vfs::FileHandleU64;

use super::super::attr::IdMap;
use super::write::DEFAULT_FILE_MODE;
use super::{MachineRoot, on_disk};

/// Sidecars kept at most; past it the oldest goes. Each is a few KiB, so
/// the table stays near 16 MiB however much the Mac writes.
const CAPACITY: usize = 4096;

/// Whether `name` is the AppleDouble sibling of another entry.
pub fn is_sidecar_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 2 && bytes.starts_with(b"._")
}

#[derive(Debug)]
struct Sidecar {
    dir: u64,
    name: OsString,
    data: Vec<u8>,
    mode: u32,
    modified: SystemTime,
}

/// The sidecars the Mac has written, by id and by place.
#[derive(Debug, Default)]
pub struct Sidecars {
    by_id: HashMap<u64, Sidecar>,
    by_name: HashMap<u64, HashMap<OsString, u64>>,
    /// Insertion order, for eviction.
    order: VecDeque<u64>,
}

impl Sidecars {
    fn lookup(&self, dir: u64, name: &OsStr) -> Option<u64> {
        self.by_name.get(&dir)?.get(name).copied()
    }

    fn contains(&self, id: u64) -> bool {
        self.by_id.contains_key(&id)
    }

    /// Starts an empty sidecar under `id`, evicting the oldest past
    /// [`CAPACITY`].
    fn insert(&mut self, id: u64, dir: u64, name: &OsStr) {
        self.by_id.insert(
            id,
            Sidecar {
                dir,
                name: name.to_owned(),
                data: Vec::new(),
                mode: DEFAULT_FILE_MODE,
                modified: SystemTime::now(),
            },
        );
        self.by_name
            .entry(dir)
            .or_default()
            .insert(name.to_owned(), id);
        self.order.push_back(id);
        while self.by_id.len() > CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(oldest);
        }
    }

    fn remove(&mut self, id: u64) -> bool {
        let Some(sidecar) = self.by_id.remove(&id) else {
            return false;
        };
        if let Some(names) = self.by_name.get_mut(&sidecar.dir) {
            names.remove(&sidecar.name);
            if names.is_empty() {
                self.by_name.remove(&sidecar.dir);
            }
        }
        true
    }

    fn remove_named(&mut self, dir: u64, name: &OsStr) -> bool {
        self.lookup(dir, name).is_some_and(|id| self.remove(id))
    }

    fn rename(&mut self, id: u64, to_dir: u64, to_name: &OsStr) {
        let Some(sidecar) = self.by_id.get_mut(&id) else {
            return;
        };
        let (from_dir, from_name) = (sidecar.dir, std::mem::take(&mut sidecar.name));
        sidecar.dir = to_dir;
        to_name.clone_into(&mut sidecar.name);
        if let Some(names) = self.by_name.get_mut(&from_dir) {
            names.remove(&from_name);
        }
        self.by_name
            .entry(to_dir)
            .or_default()
            .insert(to_name.to_owned(), id);
    }

    fn read(&self, id: u64, offset: u64, count: u32) -> Option<(Vec<u8>, bool)> {
        let data = &self.by_id.get(&id)?.data;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(count as usize).min(data.len());
        Some((data[start..end].to_vec(), end >= data.len()))
    }

    /// Writes `bytes` at `offset`, growing the sidecar with zeros to reach it.
    fn write(&mut self, id: u64, offset: u64, bytes: &[u8], map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get_mut(&id)?;
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(bytes.len())?;
        if sidecar.data.len() < end {
            sidecar.data.resize(end, 0);
        }
        sidecar.data[start..end].copy_from_slice(bytes);
        sidecar.modified = SystemTime::now();
        self.attr(id, map)
    }

    /// Applies what a sidecar can take from `attr` — size and mode — and
    /// reports the result.
    fn setattr(&mut self, id: u64, attr: &sattr3, map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get_mut(&id)?;
        if let set_mode3::Some(mode) = attr.mode {
            sidecar.mode = mode & 0o7777;
        }
        if let set_size3::Some(size) = attr.size {
            sidecar
                .data
                .resize(usize::try_from(size).unwrap_or(usize::MAX), 0);
            sidecar.modified = SystemTime::now();
        }
        self.attr(id, map)
    }

    /// The attributes of sidecar `id`: a plain file of the host user's.
    fn attr(&self, id: u64, map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get(&id)?;
        let (guest_uid, guest_gid) = map.guest_owner();
        let size = sidecar.data.len() as u64;
        let elapsed = sidecar
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time = nfstime3 {
            seconds: u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX),
            nseconds: elapsed.subsec_nanos(),
        };
        Some(fattr3 {
            type_: ftype3::NF3REG,
            mode: sidecar.mode,
            nlink: 1,
            uid: map.uid(guest_uid),
            gid: map.gid(guest_gid),
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id,
            atime: time,
            mtime: time,
            ctime: time,
        })
    }

    /// Drops every sidecar in `dir`, for the directory's removal.
    fn remove_in_dir(&mut self, dir: u64) {
        if let Some(names) = self.by_name.remove(&dir) {
            for id in names.into_values() {
                self.by_id.remove(&id);
            }
        }
    }
}

/// The sidecar hooks of the filesystem operations; each returns `None` or
/// `false` for an object that is not a sidecar, and the operation goes on
/// to disk.
impl MachineRoot {
    fn sidecars(&self) -> MutexGuard<'_, Sidecars> {
        self.sidecars.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves a `._` name: the file on disk if there is one — a sidecar
    /// the Mac wrote under that name gives way to it — else the one in
    /// memory.
    pub(super) async fn lookup_sidecar(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) -> Result<FileHandleU64, nfsstat3> {
        if on_disk(path.to_path_buf()).await? {
            self.sidecars().remove_named(dirid, name);
            return Ok(self.handle_for(dirid, name));
        }
        self.sidecars()
            .lookup(dirid, name)
            .map(FileHandleU64::new)
            .ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    pub(super) fn is_sidecar(&self, id: u64) -> bool {
        self.sidecars().contains(id)
    }

    pub(super) fn sidecar_attr(&self, id: u64) -> Option<fattr3> {
        self.sidecars().attr(id, self.map)
    }

    pub(super) fn read_sidecar(&self, id: u64, offset: u64, count: u32) -> Option<(Vec<u8>, bool)> {
        self.sidecars().read(id, offset, count)
    }

    pub(super) fn write_sidecar(&self, id: u64, offset: u64, data: &[u8]) -> Option<fattr3> {
        self.sidecars().write(id, offset, data, self.map)
    }

    pub(super) fn setattr_sidecar(&self, id: u64, attr: &sattr3) -> Option<fattr3> {
        self.sidecars().setattr(id, attr, self.map)
    }

    /// Whether creating `name` at `path` makes a sidecar: a `._` name with
    /// nothing on disk under it.
    pub(super) async fn shadows(&self, name: &OsStr, path: &Path) -> Result<bool, nfsstat3> {
        Ok(is_sidecar_name(name) && !on_disk(path.to_path_buf()).await?)
    }

    /// Creates sidecar `name` in `dirid` — unless the Mac already has, which
    /// `exclusive` refuses — and applies `attr` to it.
    pub(super) fn create_sidecar(
        &self,
        dirid: u64,
        name: &OsStr,
        attr: &sattr3,
        exclusive: bool,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        let id = self.ids().child(dirid, name);
        let mut sidecars = self.sidecars();
        if sidecars.contains(id) {
            if exclusive {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
        } else {
            sidecars.insert(id, dirid, name);
        }
        let attr = sidecars
            .setattr(id, attr, self.map)
            .ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
        Ok((FileHandleU64::new(id), attr))
    }

    /// Drops sidecar `name` from memory, telling whether there was one.
    pub(super) fn remove_sidecar(&self, dirid: u64, name: &OsStr) -> bool {
        if !self.sidecars().remove_named(dirid, name) {
            return false;
        }
        self.ids().forget(dirid, name);
        true
    }

    /// Moves a sidecar, if `from` is one: to another `._` name, in memory;
    /// to any other name, `NFS3ERR_XDEV`, so `mv` and Finder copy it out
    /// and the file lands on disk as the Mac asked.
    pub(super) fn rename_sidecar(
        &self,
        from_dirid: u64,
        from_name: &OsStr,
        to_dirid: u64,
        to_name: &OsStr,
    ) -> Option<Result<(), nfsstat3>> {
        let id = self.sidecars().lookup(from_dirid, from_name)?;
        if !is_sidecar_name(to_name) {
            return Some(Err(nfsstat3::NFS3ERR_XDEV));
        }
        self.remove_sidecar(to_dirid, to_name);
        self.sidecars().rename(id, to_dirid, to_name);
        self.ids().rename(from_dirid, from_name, to_dirid, to_name);
        Some(Ok(()))
    }

    /// Drops the sidecars under directory `name` of `dirid`, which the Mac
    /// has removed.
    pub(super) fn drop_sidecars_in(&self, dirid: u64, name: &OsStr) {
        let dir = self.ids().lookup(dirid, name);
        if let Some(dir) = dir {
            self.sidecars().remove_in_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use nfs3_server::nfs3_types::nfs3::{filename3, stable_how};
    use nfs3_server::vfs::{NextResult, NfsFileSystem, NfsReadFileSystem, ReadDirPlusIterator};

    use super::*;

    #[test]
    fn only_a_dot_underscore_prefix_with_a_name_is_a_sidecar() {
        assert!(is_sidecar_name(OsStr::new("._pack.idx")));
        assert!(!is_sidecar_name(OsStr::new("._")));
        assert!(!is_sidecar_name(OsStr::new(".hidden")));
        assert!(!is_sidecar_name(OsStr::new("pack.idx")));
    }

    #[test]
    fn the_oldest_sidecar_goes_when_the_table_is_full() {
        let mut sidecars = Sidecars::default();
        for i in 0..=CAPACITY as u64 {
            sidecars.insert(i + 10, 1, OsStr::new(&format!("._{i}")));
        }
        assert_eq!(sidecars.by_id.len(), CAPACITY);
        assert!(!sidecars.contains(10), "the first one inserted was evicted");
        assert_eq!(sidecars.lookup(1, OsStr::new("._0")), None);
        assert!(sidecars.contains(10 + CAPACITY as u64));
    }

    fn name(s: &str) -> filename3<'_> {
        filename3::from(s.as_bytes())
    }

    #[tokio::test]
    async fn the_macs_sidecars_stay_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"file").unwrap();
        std::fs::write(dir.path().join("._real"), b"machine").unwrap();
        // The guest owner is this user: `mkdir` below chowns to it.
        let me = std::fs::metadata(dir.path()).unwrap();
        let map = IdMap::new(501, 20, me.uid(), me.gid());
        let fs = MachineRoot::new(dir.path().to_path_buf(), map);
        let root = fs.root_dir();

        // Written from the Mac: in memory, served back, never on disk.
        let (sc, attr) = fs
            .create(&root, &name("._f"), sattr3::default())
            .await
            .unwrap();
        assert_eq!(
            (attr.size, attr.uid, attr.mode),
            (0, 501, DEFAULT_FILE_MODE)
        );
        assert_eq!(
            fs.create_exclusive(&root, &name("._f"), Default::default())
                .await,
            Err(nfsstat3::NFS3ERR_EXIST)
        );
        let (attr, stable) = fs
            .write(&sc, 0, b"AppleDouble", stable_how::UNSTABLE)
            .await
            .unwrap();
        assert_eq!((attr.size, stable), (11, stable_how::FILE_SYNC));
        fs.commit(&sc, 0, 0).await.unwrap();
        assert_eq!(
            fs.read(&sc, 5, 100).await.unwrap(),
            (b"Double".to_vec(), true)
        );
        assert_eq!(fs.lookup(&root, &name("._f")).await.unwrap(), sc);
        assert_eq!(fs.getattr(&sc).await.unwrap().size, 11);
        assert_eq!(fs.readlink(&sc).await, Err(nfsstat3::NFS3ERR_INVAL));
        assert!(!dir.path().join("._f").exists());

        // Not listed; the machine's own `._real` is a file like any other.
        let mut listing = fs.readdirplus(&root, 0).await.unwrap();
        let mut names = Vec::new();
        while let NextResult::Ok(entry) = listing.next().await {
            names.push(String::from_utf8(entry.name.as_ref().to_vec()).unwrap());
        }
        names.sort();
        assert_eq!(names, ["._real", "f"]);

        // One the machine made is a plain file, and create keeps it.
        let real = fs.lookup(&root, &name("._real")).await.unwrap();
        assert_eq!(fs.read(&real, 0, 100).await.unwrap().0, b"machine");
        fs.create(&root, &name("._real"), sattr3::default())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("._real")).unwrap(),
            b"machine"
        );

        // A move to a plain name is cross-device; to a sidecar name, in memory.
        assert_eq!(
            fs.rename(&root, &name("._f"), &root, &name("g")).await,
            Err(nfsstat3::NFS3ERR_XDEV)
        );
        fs.rename(&root, &name("._f"), &root, &name("._g"))
            .await
            .unwrap();
        assert_eq!(
            fs.lookup(&root, &name("._f")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        );
        assert_eq!(fs.lookup(&root, &name("._g")).await.unwrap(), sc);
        let attr = sattr3 {
            size: set_size3::Some(5),
            mode: set_mode3::Some(0o600),
            ..sattr3::default()
        };
        let attr = fs.setattr(&sc, attr).await.unwrap();
        assert_eq!((attr.size, attr.mode), (5, 0o600));
        assert_eq!(fs.read(&sc, 0, 100).await.unwrap().0, b"Apple");

        // A file the machine makes under the name takes the sidecar's place.
        std::fs::write(dir.path().join("._g"), b"disk").unwrap();
        let g = fs.lookup(&root, &name("._g")).await.unwrap();
        assert_eq!(fs.read(&g, 0, 100).await.unwrap().0, b"disk");
        std::fs::remove_file(dir.path().join("._g")).unwrap();
        assert_eq!(
            fs.lookup(&root, &name("._g")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        );

        // Removed from the Mac, a sidecar is gone and its handle stale.
        let (sc, _) = fs
            .create(&root, &name("._h"), sattr3::default())
            .await
            .unwrap();
        fs.remove(&root, &name("._h")).await.unwrap();
        assert!(matches!(
            fs.getattr(&sc).await,
            Err(nfsstat3::NFS3ERR_STALE)
        ));
        assert_eq!(
            fs.remove(&root, &name("._h")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        );

        // So are the sidecars in a directory the Mac removes.
        let (d, _) = fs.mkdir(&root, &name("d")).await.unwrap();
        let (q, _) = fs
            .create(&d, &name("._q"), sattr3::default())
            .await
            .unwrap();
        fs.remove(&root, &name("d")).await.unwrap();
        assert!(matches!(fs.getattr(&q).await, Err(nfsstat3::NFS3ERR_STALE)));
        let mut on_disk: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        on_disk.sort();
        assert_eq!(on_disk, ["._real", "f"]);
    }
}
