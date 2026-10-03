//! Reading a machine archive: the manifest, and the data disk restored
//! with its holes.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{ArchiveManifest, DATA_DISK_ENTRY, FORMAT_VERSION, MANIFEST_ENTRY, TAR_BLOCK};
use crate::error::{EngineError, Result};

/// Granularity of the zero scan that keeps a data disk arriving as a plain
/// (non-sparse) entry from being written out in full.
const ZERO_SCAN_BLOCK: usize = 64 * 1024;

type Archive = tar::Archive<zstd::stream::read::Decoder<'static, BufReader<File>>>;

pub(super) fn open(path: &Path) -> Result<Archive> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    Ok(tar::Archive::new(decoder))
}

/// Reads the manifest, the archive's first entry.
///
/// # Errors
///
/// Returns an error if `path` is not a machine archive this build reads.
pub fn read_manifest(path: &Path) -> Result<ArchiveManifest> {
    let not_an_archive = |what: String| {
        EngineError::config(format!(
            "{} is not a machine archive: {what}",
            path.display()
        ))
    };
    let mut archive = open(path)?;
    let mut entries = archive.entries()?;
    let mut entry = entries
        .next()
        .ok_or_else(|| not_an_archive("it is empty".to_owned()))?
        .map_err(|e| not_an_archive(e.to_string()))?;
    let entry_path = entry.path()?;
    if entry_path.as_ref() != Path::new(MANIFEST_ENTRY) {
        return Err(not_an_archive(format!(
            "its first entry is {}, not {MANIFEST_ENTRY}",
            entry_path.display()
        )));
    }
    let mut json = String::new();
    entry.read_to_string(&mut json)?;
    let manifest: ArchiveManifest = serde_json::from_str(&json)
        .map_err(|e| not_an_archive(format!("{MANIFEST_ENTRY} does not parse: {e}")))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(EngineError::config(format!(
            "{} uses archive format {}; this build reads format {FORMAT_VERSION}",
            path.display(),
            manifest.format_version
        )));
    }
    Ok(manifest)
}

/// Restores the archive's data disk to `dest`, holes included.
///
/// A sparse entry's extents are written at their offsets; a plain entry is
/// scanned for all-zero blocks, which are skipped rather than written.
///
/// # Errors
///
/// Returns an error if `dest` exists, the archive has no data disk, or its
/// sparse map is malformed.
pub fn extract_data_disk(path: &Path, dest: &Path) -> Result<()> {
    let mut archive = open(path)?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let sparse = sparse_pax_records(&mut entry)?;
        let name = match &sparse {
            Some((name, _)) => name.clone(),
            None => entry.path()?.to_string_lossy().into_owned(),
        };
        if name != DATA_DISK_ENTRY {
            continue;
        }
        let mut out = File::create_new(dest)?;
        match sparse {
            Some((_, realsize)) => restore_sparse(&mut entry, &mut out, realsize)?,
            None => restore_dense(&mut entry, &mut out)?,
        }
        out.sync_all()?;
        return Ok(());
    }
    Err(EngineError::config(format!(
        "{} has no {DATA_DISK_ENTRY} entry",
        path.display()
    )))
}

/// `(name, realsize)` of a GNU sparse 1.0 entry, `None` for a plain one.
pub(super) fn sparse_pax_records(
    entry: &mut tar::Entry<'_, impl Read>,
) -> Result<Option<(String, u64)>> {
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(None);
    };
    let (mut major, mut name, mut realsize) = (None, None, None);
    for extension in extensions {
        let extension = extension?;
        match extension.key() {
            Ok("GNU.sparse.major") => major = extension.value().ok().map(str::to_owned),
            Ok("GNU.sparse.name") => name = extension.value().ok().map(str::to_owned),
            Ok("GNU.sparse.realsize") => {
                realsize = extension.value().ok().and_then(|v| v.parse::<u64>().ok());
            }
            _ => {}
        }
    }
    match (major.as_deref(), name, realsize) {
        (Some("1"), Some(name), Some(realsize)) => Ok(Some((name, realsize))),
        (Some(other), _, _) => Err(EngineError::config(format!(
            "GNU sparse format {other}.x is not supported; only 1.0 is"
        ))),
        (None, _, _) => Ok(None),
    }
}

fn restore_sparse(entry: &mut impl Read, out: &mut File, realsize: u64) -> Result<()> {
    let mut reader = BufReader::new(entry);
    let mut consumed = 0usize;
    let mut number = |reader: &mut BufReader<_>| -> Result<u64> {
        let mut line = String::new();
        reader.by_ref().take(32).read_line(&mut line)?;
        consumed += line.len();
        line.trim_end_matches('\n')
            .parse::<u64>()
            .map_err(|_| EngineError::config(format!("malformed sparse map entry {line:?}")))
    };
    let count = number(&mut reader)?;
    let mut extents = Vec::with_capacity(usize::try_from(count).unwrap_or(0).min(1 << 16));
    for _ in 0..count {
        let offset = number(&mut reader)?;
        let length = number(&mut reader)?;
        extents.push((offset, length));
    }
    let padding = consumed.div_ceil(TAR_BLOCK) * TAR_BLOCK - consumed;
    io::copy(&mut reader.by_ref().take(padding as u64), &mut io::sink())?;

    for (offset, length) in extents {
        if offset.checked_add(length).is_none_or(|end| end > realsize) {
            return Err(EngineError::config(format!(
                "sparse extent {offset}+{length} lies past the disk's {realsize} bytes"
            )));
        }
        out.seek(SeekFrom::Start(offset))?;
        let copied = io::copy(&mut reader.by_ref().take(length), out)?;
        if copied != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("sparse extent {offset}+{length} ends after {copied} bytes"),
            )
            .into());
        }
    }
    out.set_len(realsize)?;
    Ok(())
}

fn restore_dense(entry: &mut impl Read, out: &mut File) -> Result<()> {
    let mut block = vec![0u8; ZERO_SCAN_BLOCK];
    let mut position = 0u64;
    loop {
        let mut filled = 0;
        while filled < block.len() {
            let read = entry.read(&mut block[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        position += filled as u64;
        if block[..filled].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Start(position))?;
        } else {
            out.write_all(&block[..filled])?;
        }
    }
    out.set_len(position)?;
    Ok(())
}
