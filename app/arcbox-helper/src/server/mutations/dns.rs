//! DNS resolver file management for `/etc/resolver/`.

mod storage;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use arcbox_constants::dns::Domain;
use arcbox_helper::HelperError;
use arcbox_helper::validate::DnsPort;

use storage::{ManagedFile, open_directory};

pub const RESOLVER_DIR: &str = "/etc/resolver";
const MARKER: &str = "# managed by arcbox-helper";

pub struct ResolverStore {
    directory: PathBuf,
    mutation: Mutex<()>,
}

impl ResolverStore {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            mutation: Mutex::new(()),
        }
    }

    pub fn install(&self, domain: &Domain, port: DnsPort) -> Result<(), HelperError> {
        let resolver_dir = &self.directory;
        let _guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        if domain.as_str() == "local" {
            return Err(HelperError::validation(
                "the .local namespace cannot be claimed",
            ));
        }
        fs::create_dir_all(resolver_dir)
            .map_err(|e| HelperError::io("create_dir", resolver_dir, e))?;
        let _directory = open_directory(resolver_dir)?.ok_or_else(|| {
            HelperError::io("open", resolver_dir, "resolver directory disappeared")
        })?;
        let path = resolver_dir.join(domain.as_str());
        if read_managed(&path)? == ManagedFile::Foreign {
            return Err(HelperError::foreign_managed(&path));
        }
        arcbox_atomic_file::write_public(&path, contents(port).as_bytes())
            .map_err(|e| HelperError::io("write", &path, e))
    }

    pub fn uninstall(&self, domain: &Domain) -> Result<(), HelperError> {
        let resolver_dir = &self.directory;
        let _guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        let Some(_directory) = open_directory(resolver_dir)? else {
            return Ok(());
        };
        let path = resolver_dir.join(domain.as_str());
        if read_managed(&path)? == ManagedFile::Owned {
            fs::remove_file(&path).map_err(|e| HelperError::io("remove", &path, e))?;
        }
        Ok(())
    }

    pub fn status(&self, domain: &Domain) -> Result<bool, HelperError> {
        let resolver_dir = &self.directory;
        let _guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        let Some(_directory) = open_directory(resolver_dir)? else {
            return Ok(false);
        };
        let path = resolver_dir.join(domain.as_str());
        Ok(read_managed(&path)? == ManagedFile::Owned)
    }
}

fn contents(port: DnsPort) -> String {
    format!("{MARKER}\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\ntimeout 5\n")
}

fn read_managed(path: &Path) -> Result<ManagedFile, HelperError> {
    storage::inspect(path, |content| {
        std::str::from_utf8(content)
            .ok()
            .and_then(|text| {
                let port = text
                    .lines()
                    .nth(2)?
                    .strip_prefix("port ")?
                    .parse::<u16>()
                    .ok()?;
                DnsPort::try_from(port).ok()
            })
            .is_some_and(|port| content == contents(port).as_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::{MARKER, ResolverStore};
    use arcbox_constants::dns::Domain;
    use arcbox_helper::HelperError;
    use arcbox_helper::validate::DnsPort;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn domain() -> Domain {
        "arcbox.local".parse().unwrap()
    }

    fn port() -> DnsPort {
        DnsPort::try_from(5553_u16).unwrap()
    }

    #[test]
    fn install_writes_marker_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let d = domain();
        let p = port();

        assert!(!store.status(&d).unwrap());
        store.install(&d, p).unwrap();
        assert!(store.status(&d).unwrap());

        let content = fs::read_to_string(dir.path().join(d.as_str())).unwrap();
        assert!(content.contains(MARKER));
        let next = DnsPort::try_from(5554).unwrap();
        store.install(&d, next).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(d.as_str())).unwrap(),
            super::contents(next)
        );
        assert_eq!(
            fs::metadata(dir.path().join(d.as_str()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn local_namespace_cannot_be_installed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        assert!(store.install(&"local".parse().unwrap(), port()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn uninstall_removes_only_marked_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let d = domain();
        store.install(&d, port()).unwrap();
        store.uninstall(&d).unwrap();
        assert!(!dir.path().join(d.as_str()).exists());
        store.uninstall(&d).unwrap();
    }

    #[test]
    fn foreign_resolver_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let d = domain();
        let path = dir.path().join(d.as_str());
        fs::write(&path, "nameserver 1.1.1.1\n").unwrap();

        let err = store.install(&d, port()).unwrap_err();
        assert!(
            matches!(err, HelperError::ForeignManagedFile { .. }),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "nameserver 1.1.1.1\n");
        assert!(!store.status(&d).unwrap());
        store.uninstall(&d).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn managed_marker_does_not_authorize_foreign_contents_or_writable_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let path = dir.path().join(domain().as_str());
        for content in [
            format!("# copied {MARKER}\nnameserver 1.1.1.1\n"),
            format!("{}nameserver 1.1.1.1\n", super::contents(port())),
            super::contents(port()).replace("port 5553", "port 53"),
            super::contents(port()).replace("port 5553", "port 05553"),
        ] {
            fs::write(&path, &content).unwrap();
            assert!(store.install(&domain(), port()).is_err());
            assert!(!store.status(&domain()).unwrap());
            store.uninstall(&domain()).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        fs::write(&path, super::contents(port())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(store.install(&domain(), port()).is_err());
        store.uninstall(&domain()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn links_and_nonregular_resolver_slots_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let target = dir.path().join("target");
        let path = dir.path().join(domain().as_str());
        for content in [Some(super::contents(port())), Some("foreign".into()), None] {
            if let Some(content) = &content {
                fs::write(&target, content).unwrap();
            }
            symlink(&target, &path).unwrap();
            assert!(store.install(&domain(), port()).is_err());
            assert!(!store.status(&domain()).unwrap());
            store.uninstall(&domain()).unwrap();
            assert_eq!(fs::read_link(&path).unwrap(), target);
            if let Some(content) = content {
                assert_eq!(fs::read_to_string(&target).unwrap(), content);
                fs::remove_file(&target).unwrap();
            }
            fs::remove_file(&path).unwrap();
        }
        fs::write(&target, super::contents(port())).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(store.install(&domain(), port()).is_err());
        store.uninstall(&domain()).unwrap();
        assert!(path.exists());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            super::contents(port())
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.install(&domain(), port()).is_err());
        store.uninstall(&domain()).unwrap();
        assert!(path.is_dir());
    }

    #[test]
    fn resolver_directory_must_be_a_private_writer_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolver");
        let resolver = ResolverStore::new(path.clone());
        symlink(target.path(), &path).unwrap();
        assert!(resolver.install(&domain(), port()).is_err());
        assert!(resolver.uninstall(&domain()).is_err());
        assert!(resolver.status(&domain()).is_err());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(resolver.install(&domain(), port()).is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn acl_grants_cannot_bypass_writer_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResolverStore::new(dir.path().to_path_buf());
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(store.install(&domain(), port()).is_err());
        assert!(store.status(&domain()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        let files = tempfile::tempdir().unwrap();
        let file_store = ResolverStore::new(files.path().to_path_buf());
        file_store.install(&domain(), port()).unwrap();
        let path = files.path().join(domain().as_str());
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow write"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(file_store.install(&domain(), port()).is_err());
        assert!(!file_store.status(&domain()).unwrap());
        file_store.uninstall(&domain()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), super::contents(port()));
    }
}
