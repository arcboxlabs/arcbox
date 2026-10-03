//! The write half of the machine root: `NfsFileSystem`.
//!
//! Objects the host creates belong to the guest owner of the `IdMap` unless
//! the request names one; `MKDIR` carries no attributes through this server,
//! so a new directory gets `DEFAULT_DIR_MODE`. Writes honour the stability
//! the client asked for and report it back, so an `UNSTABLE` write costs no
//! `fsync` and the `COMMIT` that follows is the one that does.

use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, FileExt as _, OpenOptionsExt as _, PermissionsExt as _,
};

use nfs3_server::nfs3_types::nfs3::{
    createverf3, fattr3, filename3, nfspath3, nfsstat3, sattr3, stable_how,
};
use nfs3_server::vfs::NfsFileSystem;

use super::super::attr::{apply_sattr, fattr3_from_metadata, new_object_owner};
use super::{MachineRoot, blocking, component};

/// Mode of a file the host creates without asking for one.
pub(super) const DEFAULT_FILE_MODE: u32 = 0o644;
/// Mode of a directory the host creates.
const DEFAULT_DIR_MODE: u32 = 0o755;

impl NfsFileSystem for MachineRoot {
    async fn setattr(&self, id: &Self::Handle, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let id = id.as_u64();
        if let Some(attr) = self.setattr_sidecar(id, &setattr) {
            return Ok(attr);
        }
        let path = self.path_of(id)?;
        let map = self.map;
        let meta = blocking(move || {
            apply_sattr(&path, &setattr, map)?;
            std::fs::symlink_metadata(&path)
        })
        .await?;
        Ok(fattr3_from_metadata(id, &meta, map))
    }

    async fn write(
        &self,
        id: &Self::Handle,
        offset: u64,
        data: &[u8],
        stable: stable_how,
    ) -> Result<(fattr3, stable_how), nfsstat3> {
        let id = id.as_u64();
        if let Some(attr) = self.write_sidecar(id, offset, data) {
            return Ok((attr, stable_how::FILE_SYNC));
        }
        let path = self.path_of(id)?;
        let map = self.map;
        let data = data.to_vec();
        blocking(move || {
            let file = OpenOptions::new().write(true).open(&path)?;
            file.write_all_at(&data, offset)?;
            match stable {
                stable_how::UNSTABLE => {}
                stable_how::DATA_SYNC => file.sync_data()?,
                stable_how::FILE_SYNC => file.sync_all()?,
            }
            let meta = file.metadata()?;
            Ok((fattr3_from_metadata(id, &meta, map), stable))
        })
        .await
    }

    async fn create(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        attr: sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        let dirid = dirid.as_u64();
        let name = component(filename)?;
        let path = self.child_path(dirid, &name)?;
        if self.shadows(&name, &path).await? {
            return self.create_sidecar(dirid, &name, &attr, false);
        }
        let map = self.map;
        let created = path.clone();
        blocking(move || {
            // `UNCHECKED`: an existing file is kept and gets the attributes.
            let fresh = std::fs::symlink_metadata(&created).is_err();
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(DEFAULT_FILE_MODE)
                .open(&created)?;
            if fresh {
                let (uid, gid) = new_object_owner(&attr, map);
                std::os::unix::fs::lchown(&created, Some(uid), Some(gid))?;
                // The open above was subject to the umask.
                std::fs::set_permissions(&created, Permissions::from_mode(DEFAULT_FILE_MODE))?;
            }
            apply_sattr(&created, &attr, map)
        })
        .await?;
        self.created(dirid, name, path).await
    }

    async fn create_exclusive(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        _createverf: createverf3,
    ) -> Result<Self::Handle, nfsstat3> {
        let dirid = dirid.as_u64();
        let name = component(filename)?;
        let path = self.child_path(dirid, &name)?;
        if self.shadows(&name, &path).await? {
            return self
                .create_sidecar(dirid, &name, &sattr3::default(), true)
                .map(|(handle, _)| handle);
        }
        let (uid, gid) = self.map.guest_owner();
        blocking(move || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(DEFAULT_FILE_MODE)
                .open(&path)?;
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
            std::fs::set_permissions(&path, Permissions::from_mode(DEFAULT_FILE_MODE))
        })
        .await?;
        Ok(self.handle_for(dirid, &name))
    }

    async fn mkdir(
        &self,
        dirid: &Self::Handle,
        dirname: &filename3<'_>,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        let dirid = dirid.as_u64();
        let name = component(dirname)?;
        let path = self.child_path(dirid, &name)?;
        let (uid, gid) = self.map.guest_owner();
        let created = path.clone();
        blocking(move || {
            DirBuilder::new().mode(DEFAULT_DIR_MODE).create(&created)?;
            std::os::unix::fs::lchown(&created, Some(uid), Some(gid))?;
            std::fs::set_permissions(&created, Permissions::from_mode(DEFAULT_DIR_MODE))
        })
        .await?;
        self.created(dirid, name, path).await
    }

    async fn remove(&self, dirid: &Self::Handle, filename: &filename3<'_>) -> Result<(), nfsstat3> {
        let dirid = dirid.as_u64();
        let name = component(filename)?;
        let path = self.child_path(dirid, &name)?;
        if self.remove_sidecar(dirid, &name) {
            return Ok(());
        }
        blocking(move || {
            if std::fs::symlink_metadata(&path)?.is_dir() {
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path)
            }
        })
        .await?;
        self.drop_sidecars_in(dirid, &name);
        self.ids().forget(dirid, &name);
        Ok(())
    }

    async fn rename<'a>(
        &self,
        from_dirid: &Self::Handle,
        from_filename: &filename3<'a>,
        to_dirid: &Self::Handle,
        to_filename: &filename3<'a>,
    ) -> Result<(), nfsstat3> {
        let (from_dirid, to_dirid) = (from_dirid.as_u64(), to_dirid.as_u64());
        let from_name = component(from_filename)?;
        let to_name = component(to_filename)?;
        let from = self.child_path(from_dirid, &from_name)?;
        let to = self.child_path(to_dirid, &to_name)?;
        if let Some(moved) = self.rename_sidecar(from_dirid, &from_name, to_dirid, &to_name) {
            return moved;
        }
        blocking(move || std::fs::rename(&from, &to)).await?;
        // A sidecar the rename wrote over.
        self.remove_sidecar(to_dirid, &to_name);
        self.ids()
            .rename(from_dirid, &from_name, to_dirid, &to_name);
        Ok(())
    }

    async fn symlink<'a>(
        &self,
        dirid: &Self::Handle,
        linkname: &filename3<'a>,
        symlink: &nfspath3<'a>,
        attr: &sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        let dirid = dirid.as_u64();
        let name = component(linkname)?;
        let path = self.child_path(dirid, &name)?;
        let target = OsStr::from_bytes(symlink.as_ref()).to_owned();
        let (uid, gid) = new_object_owner(attr, self.map);
        let created = path.clone();
        blocking(move || {
            std::os::unix::fs::symlink(&target, &created)?;
            std::os::unix::fs::lchown(&created, Some(uid), Some(gid))
        })
        .await?;
        self.created(dirid, name, path).await
    }

    async fn commit(&self, id: &Self::Handle, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        let id = id.as_u64();
        if self.is_sidecar(id) {
            return Ok(());
        }
        let path = self.path_of(id)?;
        blocking(move || std::fs::File::open(&path)?.sync_data()).await
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use nfs3_server::nfs3_types::nfs3::{set_gid3, set_uid3};
    use nfs3_server::vfs::{NextResult, NfsReadFileSystem, ReadDirPlusIterator};

    use super::super::super::attr::IdMap;
    use super::*;

    /// The whole read/write surface against a real directory: the host sees
    /// what the machine has, what it writes lands with the mapped owner, and
    /// a handle survives the directory above it being renamed.
    #[tokio::test]
    async fn the_export_mirrors_a_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/os-release"), b"ID=test\n").unwrap();
        std::fs::create_dir(dir.path().join("arcbox")).unwrap();
        let me = std::fs::metadata(dir.path()).unwrap();
        let map = IdMap::new(501, 20, me.uid(), me.gid());
        let fs = MachineRoot::new(dir.path().to_path_buf(), map);
        let root = fs.root_dir();

        // The hidden share is neither listed nor found.
        let mut listing = fs.readdirplus(&root, 0).await.unwrap();
        let mut names = Vec::new();
        while let NextResult::Ok(entry) = listing.next().await {
            names.push(String::from_utf8(entry.name.as_ref().to_vec()).unwrap());
        }
        assert_eq!(names, ["etc"]);
        assert_eq!(
            fs.lookup(&root, &filename3::from(&b"arcbox"[..])).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        );

        let etc = fs
            .lookup(&root, &filename3::from(&b"etc"[..]))
            .await
            .unwrap();
        let release = fs
            .lookup(&etc, &filename3::from(&b"os-release"[..]))
            .await
            .unwrap();
        let (data, eof) = fs.read(&release, 0, 1024).await.unwrap();
        assert_eq!(data, b"ID=test\n");
        assert!(eof);
        assert_eq!(
            fs.lookup(&etc, &filename3::from(&b".."[..])).await.unwrap(),
            root
        );

        // A file written from the host, as the host user, lands as the
        // guest owner and reads back through the machine's path.
        let attr = sattr3 {
            uid: set_uid3::Some(501),
            gid: set_gid3::Some(20),
            ..sattr3::default()
        };
        let (note, _) = fs
            .create(&etc, &filename3::from(&b"note"[..]), attr)
            .await
            .unwrap();
        let (written, committed) = fs
            .write(&note, 0, b"hi", stable_how::UNSTABLE)
            .await
            .unwrap();
        assert_eq!(written.size, 2);
        assert_eq!(committed, stable_how::UNSTABLE);
        fs.commit(&note, 0, 0).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("etc/note")).unwrap(), b"hi");
        assert_eq!(
            (written.uid, written.gid),
            (501, 20),
            "shown as the host user"
        );

        // Renaming the directory keeps the file's handle valid.
        fs.rename(
            &root,
            &filename3::from(&b"etc"[..]),
            &root,
            &filename3::from(&b"cfg"[..]),
        )
        .await
        .unwrap();
        let (data, _) = fs.read(&note, 0, 10).await.unwrap();
        assert_eq!(data, b"hi");
        assert!(dir.path().join("cfg/note").exists());

        fs.remove(&etc, &filename3::from(&b"note"[..]))
            .await
            .unwrap();
        assert!(matches!(
            fs.getattr(&note).await,
            Err(nfsstat3::NFS3ERR_STALE)
        ));
        assert_eq!(
            fs.remove(&etc, &filename3::from(&b"note"[..])).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        );
    }
}
