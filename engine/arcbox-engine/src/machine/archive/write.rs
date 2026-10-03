//! Writing a machine archive: the manifest entry, then the data disk as a
//! GNU sparse 1.0 entry holding only its data extents.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use chrono::Utc;

use super::{ArchiveManifest, DATA_DISK_ENTRY, MANIFEST_ENTRY, TAR_BLOCK};
use crate::error::{EngineError, Result};

/// zstd's default level: a data disk is mostly filesystem data that
/// compresses two- to three-fold, and the level that does it at disk speed.
const COMPRESSION_LEVEL: i32 = 3;

/// Writes the archive for `manifest` and `data_disk` at `path`.
///
/// The archive is assembled next to `path` and renamed into place, so an
/// interrupted export leaves nothing at `path`. Returns the archive's size.
///
/// # Errors
///
/// Returns an error if `path` exists, cannot be written, or the data disk
/// cannot be read.
pub fn write(path: &Path, manifest: &ArchiveManifest, data_disk: &Path) -> Result<u64> {
    if path.exists() {
        return Err(EngineError::already_exists(path.display().to_string()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            EngineError::config(format!("{} has no parent directory", path.display()))
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| EngineError::config(format!("{} is not a file path", path.display())))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let written = write_to(&temp, manifest, data_disk).and_then(|size| {
        std::fs::rename(&temp, path)?;
        Ok(size)
    });
    if written.is_err()
        && let Err(e) = std::fs::remove_file(&temp)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %temp.display(), error = %e, "could not remove the partial archive");
    }
    written
}

fn write_to(temp: &Path, manifest: &ArchiveManifest, data_disk: &Path) -> Result<u64> {
    let file = File::create_new(temp)?;
    let encoder = zstd::stream::write::Encoder::new(io::BufWriter::new(file), COMPRESSION_LEVEL)?;
    let mut tar = tar::Builder::new(encoder);
    let now = u64::try_from(Utc::now().timestamp()).unwrap_or(0);

    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| EngineError::Machine(format!("serialize the archive manifest: {e}")))?;
    let mut header = regular_file_header(0o644, now, json.len() as u64);
    tar.append_data(&mut header, MANIFEST_ENTRY, json.as_slice())?;

    let disk = File::open(data_disk)?;
    append_sparse(&mut tar, DATA_DISK_ENTRY, &disk, now)?;

    let encoder = tar.into_inner()?;
    let mut file = encoder
        .finish()?
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.flush()?;
    Ok(file.metadata()?.len())
}

pub(super) fn regular_file_header(mode: u32, mtime: u64, size: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header.set_size(size);
    header
}

/// Appends `file` as a GNU sparse 1.0 entry named `name`: PAX records carry
/// the format version, the name and the real size, and the entry's data is
/// the extent map — decimal `count`, then `offset` and `length` per extent,
/// one per line, padded to a block — followed by the extents themselves.
fn append_sparse(
    tar: &mut tar::Builder<impl Write>,
    name: &str,
    file: &File,
    mtime: u64,
) -> Result<()> {
    let size = file.metadata()?.len();
    let extents = data_extents(file, size)?;

    let mut map = format!("{}\n", extents.len()).into_bytes();
    for (offset, length) in &extents {
        write!(map, "{offset}\n{length}\n")?;
    }
    map.resize(map.len().div_ceil(TAR_BLOCK) * TAR_BLOCK, 0);
    let data_len: u64 = extents.iter().map(|(_, length)| length).sum();

    let realsize = size.to_string();
    tar.append_pax_extensions([
        ("GNU.sparse.major", b"1".as_slice()),
        ("GNU.sparse.minor", b"0".as_slice()),
        ("GNU.sparse.name", name.as_bytes()),
        ("GNU.sparse.realsize", realsize.as_bytes()),
    ])?;
    let mut header = regular_file_header(0o600, mtime, map.len() as u64 + data_len);
    let data = io::Cursor::new(map).chain(ExtentReader {
        file,
        extents: extents.into_iter(),
        offset: 0,
        remaining: 0,
    });
    tar.append_data(
        &mut header,
        format!("GNUSparseFile.{}/{name}", std::process::id()),
        data,
    )?;
    Ok(())
}

/// The `(offset, length)` extents of `file` that hold data, in order, from
/// `SEEK_DATA`/`SEEK_HOLE`. A file without holes is one extent.
pub(super) fn data_extents(file: &File, size: u64) -> io::Result<Vec<(u64, u64)>> {
    let seek = |offset: u64, whence: libc::c_int| -> io::Result<Option<u64>> {
        let offset = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
        // SAFETY: lseek on an open descriptor with no pointers involved.
        match unsafe { libc::lseek(file.as_raw_fd(), offset, whence) } {
            -1 => {
                let err = io::Error::last_os_error();
                // ENXIO: no data (or hole) past `offset` — the end of the scan.
                if err.raw_os_error() == Some(libc::ENXIO) {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
            end => Ok(Some(u64::try_from(end).unwrap_or(0))),
        }
    };
    let mut extents = Vec::new();
    let mut position = 0;
    while position < size {
        let Some(data) = seek(position, libc::SEEK_DATA)? else {
            break;
        };
        // Every file ends in an implicit hole, so this never runs out.
        let hole = seek(data, libc::SEEK_HOLE)?.unwrap_or(size);
        extents.push((data, hole - data));
        position = hole;
    }
    Ok(extents)
}

/// Reads a file's data extents back to back, as the sparse entry stores
/// them. Exactly the promised bytes are produced: a file that shrank in
/// between is an error, not a short entry — tar does not check the length.
struct ExtentReader<'a> {
    file: &'a File,
    extents: std::vec::IntoIter<(u64, u64)>,
    offset: u64,
    remaining: u64,
}

impl Read for ExtentReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.remaining == 0 {
            let Some((offset, length)) = self.extents.next() else {
                return Ok(0);
            };
            self.offset = offset;
            self.remaining = length;
        }
        let want = usize::try_from(self.remaining).map_or(buf.len(), |r| r.min(buf.len()));
        let read = self.file.read_at(&mut buf[..want], self.offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the data disk shrank while it was being archived",
            ));
        }
        self.offset += read as u64;
        self.remaining -= read as u64;
        Ok(read)
    }
}
