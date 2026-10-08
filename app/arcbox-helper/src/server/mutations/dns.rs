//! DNS resolver file management for `/etc/resolver/`.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Mutex;

use arcbox_constants::dns::Domain;
use arcbox_helper::HelperError;
use arcbox_helper::validate::DnsPort;

use crate::server::fs_root;

const RESOLVER_DIR: &str = "/etc/resolver";
const MARKER: &str = "# managed by arcbox-helper";
static MUTATION: Mutex<()> = Mutex::new(());

pub fn install(domain: &Domain, port: DnsPort) -> Result<(), HelperError> {
    install_at(&fs_root::resolve(RESOLVER_DIR), domain, port)
}

pub fn uninstall(domain: &Domain) -> Result<(), HelperError> {
    uninstall_at(&fs_root::resolve(RESOLVER_DIR), domain)
}

pub fn status(domain: &Domain) -> Result<bool, HelperError> {
    status_at(&fs_root::resolve(RESOLVER_DIR), domain)
}

fn install_at(resolver_dir: &Path, domain: &Domain, port: DnsPort) -> Result<(), HelperError> {
    let _guard = MUTATION
        .lock()
        .expect("DNS resolver mutation lock poisoned");
    if domain.as_str() == "local" {
        return Err(HelperError::validation(
            "the .local namespace cannot be claimed",
        ));
    }
    fs::create_dir_all(resolver_dir).map_err(|e| HelperError::io("create_dir", resolver_dir, e))?;
    let _directory = open_directory(resolver_dir)?
        .ok_or_else(|| HelperError::io("open", resolver_dir, "resolver directory disappeared"))?;
    let path = resolver_dir.join(domain.as_str());
    if read_managed(&path)? == ManagedFile::Foreign {
        return Err(HelperError::foreign_managed(&path));
    }
    arcbox_atomic_file::write_public(&path, contents(port).as_bytes())
        .map_err(|e| HelperError::io("write", &path, e))
}

fn uninstall_at(resolver_dir: &Path, domain: &Domain) -> Result<(), HelperError> {
    let _guard = MUTATION
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

fn status_at(resolver_dir: &Path, domain: &Domain) -> Result<bool, HelperError> {
    let _guard = MUTATION
        .lock()
        .expect("DNS resolver mutation lock poisoned");
    let Some(_directory) = open_directory(resolver_dir)? else {
        return Ok(false);
    };
    let path = resolver_dir.join(domain.as_str());
    Ok(read_managed(&path)? == ManagedFile::Owned)
}

fn contents(port: DnsPort) -> String {
    format!("{MARKER}\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\ntimeout 5\n")
}

fn open_directory(path: &Path) -> Result<Option<File>, HelperError> {
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HelperError::io("open directory", path, error)),
    };
    let metadata = directory
        .metadata()
        .map_err(|e| HelperError::io("stat", path, e))?;
    if !private_writer(&metadata) || has_acl(&directory, path)? {
        return Err(HelperError::foreign_managed(path));
    }
    Ok(Some(directory))
}

#[derive(PartialEq, Eq)]
enum ManagedFile {
    Missing,
    Foreign,
    Owned,
}

fn private_writer(metadata: &fs::Metadata) -> bool {
    // SAFETY: geteuid takes no arguments and has no failure case.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0
}

fn has_acl(file: &File, path: &Path) -> Result<bool, HelperError> {
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            entry: libc::c_int,
            result: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    // SAFETY: the descriptor is live; 0x100 is macOS ACL_TYPE_EXTENDED.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        // macOS reports a missing ACL as ENOENT for a live descriptor.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(HelperError::io("read ACL", path, error))
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: acl is valid, 0 is ACL_FIRST_ENTRY, and entry is a writable pointer.
    let result = unsafe { acl_get_entry(acl, 0, &raw mut entry) };
    let error = std::io::Error::last_os_error();
    // SAFETY: acl was allocated by acl_get_fd_np and is no longer used.
    unsafe { acl_free(acl) };
    match result {
        0 => Ok(true),
        // macOS returns EINVAL when a valid ACL has no first entry.
        -1 if error.raw_os_error() == Some(libc::EINVAL) => Ok(false),
        _ => Err(HelperError::io("read ACL entry", path, error)),
    }
}

fn read_managed(path: &Path) -> Result<ManagedFile, HelperError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagedFile::Missing);
        }
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(ManagedFile::Foreign),
        Err(error) => return Err(HelperError::io("open", path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|e| HelperError::io("stat", path, e))?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || !private_writer(&metadata)
        || metadata.len() > 256
        || has_acl(&file, path)?
    {
        return Ok(ManagedFile::Foreign);
    }
    let mut content = Vec::new();
    file.take(257)
        .read_to_end(&mut content)
        .map_err(|e| HelperError::io("read", path, e))?;
    let owned = std::str::from_utf8(&content)
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
        .is_some_and(|port| content == contents(port).as_bytes());
    Ok(if owned {
        ManagedFile::Owned
    } else {
        ManagedFile::Foreign
    })
}

#[cfg(test)]
mod tests {
    use super::{MARKER, install_at, status_at, uninstall_at};
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
        let d = domain();
        let p = port();

        assert!(!status_at(dir.path(), &d).unwrap());
        install_at(dir.path(), &d, p).unwrap();
        assert!(status_at(dir.path(), &d).unwrap());

        let content = fs::read_to_string(dir.path().join(d.as_str())).unwrap();
        assert!(content.contains(MARKER));
        let next = DnsPort::try_from(5554).unwrap();
        install_at(dir.path(), &d, next).unwrap();
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
        assert!(install_at(dir.path(), &"local".parse().unwrap(), port()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn uninstall_removes_only_marked_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = domain();
        install_at(dir.path(), &d, port()).unwrap();
        uninstall_at(dir.path(), &d).unwrap();
        assert!(!dir.path().join(d.as_str()).exists());
        uninstall_at(dir.path(), &d).unwrap();
    }

    #[test]
    fn foreign_resolver_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let d = domain();
        let path = dir.path().join(d.as_str());
        fs::write(&path, "nameserver 1.1.1.1\n").unwrap();

        let err = install_at(dir.path(), &d, port()).unwrap_err();
        assert!(
            matches!(err, HelperError::ForeignManagedFile { .. }),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "nameserver 1.1.1.1\n");
        assert!(!status_at(dir.path(), &d).unwrap());
        uninstall_at(dir.path(), &d).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn managed_marker_does_not_authorize_foreign_contents_or_writable_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(domain().as_str());
        for content in [
            format!("# copied {MARKER}\nnameserver 1.1.1.1\n"),
            format!("{}nameserver 1.1.1.1\n", super::contents(port())),
            super::contents(port()).replace("port 5553", "port 53"),
            super::contents(port()).replace("port 5553", "port 05553"),
        ] {
            fs::write(&path, &content).unwrap();
            assert!(install_at(dir.path(), &domain(), port()).is_err());
            assert!(!status_at(dir.path(), &domain()).unwrap());
            uninstall_at(dir.path(), &domain()).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        fs::write(&path, super::contents(port())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(install_at(dir.path(), &domain(), port()).is_err());
        uninstall_at(dir.path(), &domain()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn links_and_nonregular_resolver_slots_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let path = dir.path().join(domain().as_str());
        for content in [Some(super::contents(port())), Some("foreign".into()), None] {
            if let Some(content) = &content {
                fs::write(&target, content).unwrap();
            }
            symlink(&target, &path).unwrap();
            assert!(install_at(dir.path(), &domain(), port()).is_err());
            assert!(!status_at(dir.path(), &domain()).unwrap());
            uninstall_at(dir.path(), &domain()).unwrap();
            assert_eq!(fs::read_link(&path).unwrap(), target);
            if let Some(content) = content {
                assert_eq!(fs::read_to_string(&target).unwrap(), content);
                fs::remove_file(&target).unwrap();
            }
            fs::remove_file(&path).unwrap();
        }
        fs::write(&target, super::contents(port())).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(install_at(dir.path(), &domain(), port()).is_err());
        uninstall_at(dir.path(), &domain()).unwrap();
        assert!(path.exists());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            super::contents(port())
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(install_at(dir.path(), &domain(), port()).is_err());
        uninstall_at(dir.path(), &domain()).unwrap();
        assert!(path.is_dir());
    }

    #[test]
    fn resolver_directory_must_be_a_private_writer_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolver");
        symlink(target.path(), &path).unwrap();
        assert!(install_at(&path, &domain(), port()).is_err());
        assert!(uninstall_at(&path, &domain()).is_err());
        assert!(status_at(&path, &domain()).is_err());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(install_at(&path, &domain(), port()).is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn acl_grants_cannot_bypass_writer_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(install_at(dir.path(), &domain(), port()).is_err());
        assert!(status_at(dir.path(), &domain()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        let files = tempfile::tempdir().unwrap();
        install_at(files.path(), &domain(), port()).unwrap();
        let path = files.path().join(domain().as_str());
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow write"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(install_at(files.path(), &domain(), port()).is_err());
        assert!(!status_at(files.path(), &domain()).unwrap());
        uninstall_at(files.path(), &domain()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), super::contents(port()));
    }
}
