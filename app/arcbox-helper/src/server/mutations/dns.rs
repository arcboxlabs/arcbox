//! DNS resolver file management for `/etc/resolver/`.

mod authority;
pub mod lease;
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
    mutation: Mutex<lease::State>,
    authority_epoch: tokio::sync::watch::Sender<u64>,
}

impl ResolverStore {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            mutation: Mutex::new(lease::State::default()),
            authority_epoch: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn install(&self, domain: &Domain, port: DnsPort) -> Result<(), HelperError> {
        let resolver_dir = &self.directory;
        let guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        guard.unleased(domain)?;
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
        let guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        guard.unleased(domain)?;
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
