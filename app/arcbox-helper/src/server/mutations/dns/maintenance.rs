//! Offline resolver inspection and explicit installation migration.

#[cfg(test)]
mod tests;

use std::os::unix::fs::MetadataExt;
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
        .map(|candidate| candidate.root.to_string())
        .collect())
}

/// Stops the helper and removes selected inactive legacy resolvers as one checked batch.
pub fn prepare_install(roots: &[String]) -> Result<(), HelperError> {
    // SAFETY: geteuid has no arguments or failure case.
    if unsafe { libc::geteuid() } != 0 {
        return Err(HelperError::validation(
            "helper installation preparation requires root",
        ));
    }
    let roots = parse_roots(roots)?;
    migrate(
        Path::new(RESOLVER_DIR),
        0,
        &roots,
        crate::install::stop_helper,
    )
}

#[derive(Debug, PartialEq, Eq)]
struct LegacyResolver {
    root: Domain,
    port: DnsPort,
    contents: Vec<u8>,
    device: u64,
    inode: u64,
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
) -> Result<Vec<LegacyResolver>, HelperError> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let Some(_directory) = storage::open_directory(directory, expected_owner)? else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for root in roots {
        let mut candidate = None;
        if storage::inspect(
            &directory.join(root.as_str()),
            expected_owner,
            |bytes, metadata| {
                candidate = legacy_port(bytes).map(|port| LegacyResolver {
                    root: root.clone(),
                    port,
                    contents: bytes.to_vec(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                });
                candidate.is_some()
            },
        )? == ManagedFile::Owned
        {
            candidates.push(candidate.expect("owned legacy content has a port"));
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

fn migrate(
    directory: &Path,
    expected_owner: u32,
    roots: &[Domain],
    stop_helper: impl FnOnce() -> Result<(), HelperError>,
) -> Result<(), HelperError> {
    let candidates = inspect_legacy(directory, expected_owner, roots)?;
    let ports = candidates
        .iter()
        .map(|candidate| candidate.port.value())
        .collect::<std::collections::BTreeSet<_>>();
    let mut reservations = Vec::with_capacity(ports.len());
    for port in ports {
        reservations.push(
            std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port))
                .map_err(|error| HelperError::io("reserve legacy DNS port", directory, error))?,
        );
    }
    stop_helper()?;
    if inspect_legacy(directory, expected_owner, roots)? != candidates {
        return Err(HelperError::other(
            "selected legacy resolvers changed while stopping the helper",
        ));
    }
    for candidate in candidates {
        let path = directory.join(candidate.root.as_str());
        std::fs::remove_file(&path)
            .map_err(|error| HelperError::io("remove legacy resolver", path, error))?;
    }
    drop(reservations);
    Ok(())
}
