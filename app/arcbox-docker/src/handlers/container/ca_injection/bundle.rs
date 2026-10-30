//! Replace only the validated ArcBox section of a container's PEM trust bundle.

use std::{io::Read, path::Path};

use arcbox_local_ca::{PublicAuthority, reconcile_trust_bundle};

use crate::error::{DockerError, Result};

pub(super) fn reconcile(bundle: &[u8], pem: &[u8]) -> Result<Vec<u8>> {
    let current = PublicAuthority::from_pem(pem)
        .map_err(|e| DockerError::Server(format!("invalid published ArcBox CA: {e}")))?;
    reconcile_trust_bundle(bundle, Some(&current))
        .map_err(|e| DockerError::Server(format!("invalid container CA bundle: {e}")))
}

pub(super) fn reconcile_archive(bytes: &[u8], path: &Path, pem: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut archive = tar::Archive::new(bytes);
    let mut entries = archive.entries()?;
    let mut entry = entries
        .next()
        .ok_or_else(|| DockerError::Server("empty container CA archive".into()))??;
    if !entry.header().entry_type().is_file()
        || entry.path()?.as_ref() != Path::new(path.file_name().expect("bundle paths name a file"))
    {
        return Err(DockerError::Server(
            "container CA archive is not the requested regular file".into(),
        ));
    }
    let mut content = Vec::new();
    entry.read_to_end(&mut content)?;
    let mut header = entry.header().clone();
    let replacement = reconcile(&content, pem)?;
    if entries.next().is_some() {
        return Err(DockerError::Server(
            "container CA archive contains extra files".into(),
        ));
    }
    if replacement == content {
        return Ok(None);
    }
    header.set_path(path.file_name().expect("bundle paths name a file"))?;
    header.set_size(replacement.len() as u64);
    header.set_cksum();
    let mut output = tar::Builder::new(Vec::new());
    output.append(&header, replacement.as_slice())?;
    Ok(Some(output.into_inner()?))
}

#[cfg(test)]
mod tests;
