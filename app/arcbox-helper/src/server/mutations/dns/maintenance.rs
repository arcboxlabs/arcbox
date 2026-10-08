//! Offline checks for the installer; runtime leases never adopt legacy files.

#[cfg(test)]
mod tests;

use std::path::Path;

use arcbox_constants::dns::Domain;
use arcbox_helper::{HelperError, validate::DnsPort};

use super::{
    RESOLVER_DIR, contents,
    storage::{self, ManagedFile},
};

/// Lists selected roots whose files match a supported legacy resolver format.
pub fn check_resolvers(roots: &[String]) -> Result<Vec<String>, HelperError> {
    let roots = parse_roots(roots)?;
    Ok(inspect_legacy(Path::new(RESOLVER_DIR), 0, &roots)?
        .into_iter()
        .map(|(root, _)| root.to_string())
        .collect())
}

fn parse_roots(roots: &[String]) -> Result<Vec<Domain>, HelperError> {
    let mut roots = roots
        .iter()
        .map(|root| root.parse::<Domain>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(HelperError::validation)?;
    if roots.iter().any(|root| root.as_str() == "local") {
        return Err(HelperError::validation(
            "the .local namespace cannot be claimed",
        ));
    }
    roots.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    roots.dedup();
    Ok(roots)
}

fn inspect_legacy(
    directory: &Path,
    expected_owner: u32,
    roots: &[Domain],
) -> Result<Vec<(Domain, DnsPort)>, HelperError> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let Some(_directory) = storage::open_directory(directory, expected_owner)? else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for root in roots {
        let mut port = None;
        if storage::inspect(&directory.join(root.as_str()), expected_owner, |bytes| {
            port = legacy_port(bytes);
            port.is_some()
        })? == ManagedFile::Owned
        {
            candidates.push((root.clone(), port.expect("owned legacy content has a port")));
        }
    }
    Ok(candidates)
}

fn legacy_port(bytes: &[u8]) -> Option<DnsPort> {
    let text = std::str::from_utf8(bytes).ok()?;
    let port = text
        .lines()
        .nth(2)?
        .strip_prefix("port ")?
        .parse::<u16>()
        .ok()?;
    let port = DnsPort::try_from(port).ok()?;
    let cli = format!("# managed by arcbox\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\n");
    (bytes == contents(port).as_bytes() || bytes == cli.as_bytes()).then_some(port)
}
