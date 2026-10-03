//! The machine's root as an NFSv3 filesystem.
//!
//! Every operation resolves its handle to a path through the [`IdTable`]
//! and then does what the host asked with plain `std::fs` calls, as root
//! and without following symlinks, on tokio's blocking pool. The agent is
//! root, so permission bits never refuse an operation here: the host's
//! machine is the host's. Ownership on the wire goes through the
//! [`IdMap`]; see `attr`.
//!
//! The one thing the export hides is `/arcbox`, ArcBox's own VirtioFS
//! share of the host's data directory: exporting it would hand the host
//! its own disk images back through two filesystems, and Finder or `du`
//! would happily read them. The one thing it keeps out is the Mac's
//! AppleDouble `._` files; see `sidecar`.

mod sidecar;
mod write;

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use nfs3_server::nfs3_types::nfs3::{fattr3, filename3, nfspath3, nfsstat3};
use nfs3_server::vfs::{
    DirEntryPlus, FileHandleU64, NextResult, NfsReadFileSystem, ReadDirPlusIterator,
};

use self::sidecar::{Sidecars, is_sidecar_name};
use super::attr::{IdMap, fattr3_from_metadata, nfs_error};
use super::ids::{IdTable, ROOT};

/// Root entries the export does not show; see the module docs.
const HIDDEN_ROOT_ENTRIES: &[&str] = &["arcbox"];

/// The machine root served over NFS.
pub struct MachineRoot {
    root: PathBuf,
    ids: Mutex<IdTable>,
    sidecars: Mutex<Sidecars>,
    map: IdMap,
}

impl MachineRoot {
    pub fn new(root: PathBuf, map: IdMap) -> Self {
        Self {
            root,
            ids: Mutex::new(IdTable::new()),
            sidecars: Mutex::new(Sidecars::default()),
            map,
        }
    }

    fn ids(&self) -> MutexGuard<'_, IdTable> {
        // The table is only ever mutated between awaits, so a panic while
        // holding the lock leaves it consistent enough to keep serving.
        self.ids.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The path a handle names, or `NFS3ERR_STALE` for one the table has
    /// never issued or has forgotten.
    fn path_of(&self, id: u64) -> Result<PathBuf, nfsstat3> {
        self.ids()
            .path(&self.root, id)
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    /// The path of `name` inside directory `dirid`; a hidden root entry
    /// does not exist as far as the host can tell.
    fn child_path(&self, dirid: u64, name: &OsStr) -> Result<PathBuf, nfsstat3> {
        if is_hidden(dirid, name) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        Ok(self.path_of(dirid)?.join(name))
    }

    fn handle_for(&self, dirid: u64, name: &OsStr) -> FileHandleU64 {
        FileHandleU64::new(self.ids().child(dirid, name))
    }

    /// Resolves a freshly created object: its id and its attributes.
    async fn created(
        &self,
        dirid: u64,
        name: OsString,
        path: PathBuf,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        let map = self.map;
        let meta = blocking(move || std::fs::symlink_metadata(&path)).await?;
        let id = self.ids().child(dirid, &name);
        Ok((FileHandleU64::new(id), fattr3_from_metadata(id, &meta, map)))
    }
}

fn is_hidden(dirid: u64, name: &OsStr) -> bool {
    dirid == ROOT
        && HIDDEN_ROOT_ENTRIES
            .iter()
            .any(|hidden| OsStr::new(hidden) == name)
}

/// A filename the host sent, as a path component: one component, so no
/// separator, no NUL, and neither of the two directory aliases, which
/// `lookup` resolves itself and nothing else may name.
fn component(name: &filename3<'_>) -> Result<OsString, nfsstat3> {
    let bytes = name.as_ref();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    Ok(OsStr::from_bytes(bytes).to_owned())
}

/// Whether anything is at `path`, a symlink included.
async fn on_disk(path: PathBuf) -> Result<bool, nfsstat3> {
    match blocking(move || std::fs::symlink_metadata(&path)).await {
        Ok(_) => Ok(true),
        Err(nfsstat3::NFS3ERR_NOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Runs a filesystem operation on the blocking pool.
async fn blocking<T, F>(op: F) -> Result<T, nfsstat3>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(op).await {
        Ok(result) => result.map_err(|e| nfs_error(&e)),
        Err(e) => {
            tracing::error!(error = %e, "machine export: filesystem task failed");
            Err(nfsstat3::NFS3ERR_SERVERFAULT)
        }
    }
}

/// One directory listing, sorted by id so a client can resume after the
/// last cookie it saw even when the directory changed in between.
pub struct Listing {
    entries: std::vec::IntoIter<DirEntryPlus<FileHandleU64>>,
}

impl ReadDirPlusIterator<FileHandleU64> for Listing {
    async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
        self.entries.next().map_or(NextResult::Eof, NextResult::Ok)
    }
}

impl NfsReadFileSystem for MachineRoot {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> Self::Handle {
        FileHandleU64::new(ROOT)
    }

    async fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> Result<Self::Handle, nfsstat3> {
        let dirid = dirid.as_u64();
        match filename.as_ref() {
            b"." => return Ok(FileHandleU64::new(dirid)),
            b".." => {
                return self
                    .ids()
                    .parent(dirid)
                    .map(FileHandleU64::new)
                    .ok_or(nfsstat3::NFS3ERR_STALE);
            }
            _ => {}
        }
        let name = component(filename)?;
        let path = self.child_path(dirid, &name)?;
        if is_sidecar_name(&name) {
            return self.lookup_sidecar(dirid, &name, &path).await;
        }
        blocking(move || std::fs::symlink_metadata(&path)).await?;
        Ok(self.handle_for(dirid, &name))
    }

    async fn getattr(&self, id: &Self::Handle) -> Result<fattr3, nfsstat3> {
        let id = id.as_u64();
        if let Some(attr) = self.sidecar_attr(id) {
            return Ok(attr);
        }
        let path = self.path_of(id)?;
        let map = self.map;
        let meta = blocking(move || std::fs::symlink_metadata(&path)).await?;
        Ok(fattr3_from_metadata(id, &meta, map))
    }

    async fn read(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let id = id.as_u64();
        if let Some(read) = self.read_sidecar(id, offset, count) {
            return Ok(read);
        }
        let path = self.path_of(id)?;
        blocking(move || {
            let file = std::fs::File::open(&path)?;
            let len = file.metadata()?.len();
            let available = len.saturating_sub(offset).min(u64::from(count));
            let mut buf = vec![
                0u8;
                usize::try_from(available)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?
            ];
            let mut filled = 0;
            while filled < buf.len() {
                let n = file.read_at(&mut buf[filled..], offset + filled as u64)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            buf.truncate(filled);
            let eof = offset.saturating_add(filled as u64) >= len;
            Ok((buf, eof))
        })
        .await
    }

    async fn readdirplus(
        &self,
        dirid: &Self::Handle,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        let dirid = dirid.as_u64();
        let dir = self.path_of(dirid)?;
        let entries = blocking(move || {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                // `DirEntry::metadata` does not follow symlinks.
                out.push((entry.file_name(), entry.metadata()?));
            }
            Ok(out)
        })
        .await?;
        let map = self.map;
        let mut listing: Vec<DirEntryPlus<FileHandleU64>> = {
            let mut ids = self.ids();
            entries
                .into_iter()
                .filter(|(name, _)| !is_hidden(dirid, name))
                .map(|(name, meta)| {
                    let id = ids.child(dirid, &name);
                    DirEntryPlus {
                        fileid: id,
                        name: filename3::from(name.into_vec()),
                        cookie: id,
                        name_attributes: Some(fattr3_from_metadata(id, &meta, map)),
                        name_handle: Some(FileHandleU64::new(id)),
                    }
                })
                .filter(|entry| entry.cookie > cookie)
                .collect()
        };
        listing.sort_by_key(|entry| entry.cookie);
        Ok(Listing {
            entries: listing.into_iter(),
        })
    }

    async fn readlink(&self, id: &Self::Handle) -> Result<nfspath3<'_>, nfsstat3> {
        let id = id.as_u64();
        if self.is_sidecar(id) {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        let path = self.path_of(id)?;
        let target = blocking(move || std::fs::read_link(&path)).await?;
        Ok(nfspath3::from(target.into_os_string().into_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_component_is_one_plain_name() {
        for ok in ["hosts", "a b", ".hidden", "é"] {
            assert!(component(&filename3::from(ok.as_bytes())).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert_eq!(
                component(&filename3::from(bad.as_bytes())),
                Err(nfsstat3::NFS3ERR_INVAL),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_the_root_hides_arcbox() {
        assert!(is_hidden(ROOT, OsStr::new("arcbox")));
        assert!(!is_hidden(ROOT, OsStr::new("etc")));
        assert!(!is_hidden(ROOT + 1, OsStr::new("arcbox")));
    }

    #[tokio::test]
    async fn reads_past_the_end_are_short_and_final() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"abcdef").unwrap();
        let fs = MachineRoot::new(dir.path().to_path_buf(), IdMap::new(1, 1, 0, 0));
        let f = fs
            .lookup(&fs.root_dir(), &filename3::from(&b"f"[..]))
            .await
            .unwrap();
        assert_eq!(fs.read(&f, 4, 10).await.unwrap(), (b"ef".to_vec(), true));
        assert_eq!(fs.read(&f, 0, 3).await.unwrap(), (b"abc".to_vec(), false));
        assert_eq!(fs.read(&f, 100, 3).await.unwrap(), (Vec::new(), true));
    }
}
