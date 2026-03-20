//! DNS resolver file management for `/etc/resolver/`.
//!
//! Creates or removes per-domain resolver files that point macOS's
//! `mDNSResponder` to the ArcBox DNS server running on localhost.

use std::fs;
use std::path::PathBuf;

use arcbox_helper::validate;

/// Directory where macOS looks for per-domain resolver files.
const RESOLVER_DIR: &str = "/etc/resolver";

/// Marker comment to identify files managed by ArcBox.
const MARKER: &str = "# managed by arcbox-helper";

/// Resolver file path for a given domain.
fn resolver_path(domain: &str) -> PathBuf {
    PathBuf::from(RESOLVER_DIR).join(domain)
}

/// Installs a resolver file for `domain` pointing to `127.0.0.1:port`.
///
/// Creates `/etc/resolver/<domain>` with a nameserver entry.
/// Idempotent: overwrites if the file already exists.
pub fn install(domain: &str, port: u16) -> Result<(), String> {
    validate::validate_domain(domain)?;
    validate::validate_port(port)?;

    // Ensure /etc/resolver/ exists.
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
pub fn uninstall(domain: &str) -> Result<(), String> {
    validate::validate_domain(domain)?;

    let path = resolver_path(domain);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

/// Checks if a resolver file is installed for `domain`.
pub fn status(domain: &str) -> Result<bool, String> {
    validate::validate_domain(domain)?;

    let path = resolver_path(domain);
    match fs::read_to_string(&path) {
        Ok(content) => Ok(content.contains(MARKER) || content.contains("nameserver")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}
