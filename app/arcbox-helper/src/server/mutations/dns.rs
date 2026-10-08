//! DNS resolver file management for `/etc/resolver/`.

mod authority;
pub mod lease;
mod storage;

use std::path::PathBuf;
use std::sync::Mutex;

use arcbox_helper::validate::DnsPort;

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
}

fn contents(port: DnsPort) -> String {
    format!("{MARKER}\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\ntimeout 5\n")
}
