//! DNS resolver file management for `/etc/resolver/`.
//!
//! Creates or removes per-domain resolver files that point macOS's
//! `mDNSResponder` to the ArcBox DNS server running on localhost.

use std::fs;
use std::path::PathBuf;

use arcbox_helper::validate::{DnsPort, Domain};

/// Directory where macOS looks for per-domain resolver files.
const RESOLVER_DIR: &str = "/etc/resolver";

/// Marker comment to identify files managed by ArcBox.
const MARKER: &str = "# managed by arcbox-helper";

fn resolver_path(domain: &Domain) -> PathBuf {
    PathBuf::from(RESOLVER_DIR).join(domain.as_str())
}

/// Installs a resolver file for `domain` pointing to `127.0.0.1:port`.
///
/// Creates `/etc/resolver/<domain>` with a nameserver entry.
/// Idempotent: overwrites if the file already exists.
pub fn install(domain: &Domain, port: DnsPort) -> Result<(), String> {
    fs::create_dir_all(RESOLVER_DIR)
        .map_err(|e| format!("failed to create {RESOLVER_DIR}: {e}"))?;

    let content =
        format!("{MARKER}\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\ntimeout 5\n");

    let path = resolver_path(domain);
    fs::write(&path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Removes the resolver file for `domain`.
///
/// Idempotent: returns Ok if the file does not exist.
pub fn uninstall(domain: &Domain) -> Result<(), String> {
    let path = resolver_path(domain);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

/// Checks if a resolver file is installed for `domain`.
pub fn status(domain: &Domain) -> Result<bool, String> {
    let path = resolver_path(domain);
    match fs::read_to_string(&path) {
        Ok(content) => Ok(content.contains(MARKER) || content.contains("nameserver")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}
