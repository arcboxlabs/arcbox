//! Replace only the validated ArcBox section of a container's PEM trust bundle.

use std::{io::Read, path::Path};

use arcbox_local_ca::PublicAuthority;

use crate::error::{DockerError, Result};

const BEGIN: &str = "\n# BEGIN ARCBOX LOCAL CA\n";
const END: &str = "# END ARCBOX LOCAL CA\n";

pub(super) fn reconcile(bundle: &[u8], pem: &[u8]) -> Result<Vec<u8>> {
    let current = PublicAuthority::from_pem(pem)
        .map_err(|e| DockerError::Server(format!("invalid published ArcBox CA: {e}")))?;
    let text = std::str::from_utf8(bundle)
        .map_err(|e| DockerError::Server(format!("container CA bundle is not PEM text: {e}")))?;
    let starts: Vec<_> = text.match_indices(BEGIN).map(|(index, _)| index).collect();
    let ends: Vec<_> = text.match_indices(END).map(|(index, _)| index).collect();
    let (before, after) = match (starts.as_slice(), ends.as_slice()) {
        ([], []) => (text, ""),
        ([start], [end]) if start + BEGIN.len() <= *end => {
            let previous = PublicAuthority::from_pem(&text.as_bytes()[start + BEGIN.len()..*end])
                .map_err(|e| {
                DockerError::Server(format!("invalid ArcBox section in CA bundle: {e}"))
            })?;
            if previous.der() == current.der() {
                return Ok(bundle.to_vec());
            }
            (&text[..*start], &text[end + END.len()..])
        }
        _ => {
            return Err(DockerError::Server(
                "ambiguous ArcBox markers in CA bundle".into(),
            ));
        }
    };
    Ok([
        before.as_bytes(),
        BEGIN.as_bytes(),
        pem.trim_ascii(),
        b"\n",
        END.as_bytes(),
        after.as_bytes(),
    ]
    .concat())
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
