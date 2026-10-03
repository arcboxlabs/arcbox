use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use chrono::Utc;

use super::read::{open, sparse_pax_records};
use super::write::{data_extents, regular_file_header};
use super::*;
use crate::error::EngineError;

fn manifest() -> ArchiveManifest {
    let image: MachineImageManifest = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "name": "alpine-3.24-arm64",
        "version": "20260716_1300",
        "distro": "alpine",
        "release": "3.24",
        "release_title": "3.24",
        "arch": "arm64",
        "variant": "default",
        "upstream": {
            "server": "https://images.linuxcontainers.org",
            "product": "alpine:3.24:arm64:default",
            "version": "20260716_13:00"
        },
        "rootfs": { "path": "rootfs.squashfs", "format": "squashfs", "size": 7, "sha256": "ab" }
    }))
    .unwrap();
    ArchiveManifest {
        format_version: FORMAT_VERSION,
        arcbox_version: "test".to_owned(),
        exported_at: Utc::now(),
        machine: ArchivedMachine {
            name: "dev".to_owned(),
            cpus: 2,
            memory_mb: 2048,
            disk_gb: 1,
            distro: "alpine".to_owned(),
            distro_version: Some("3.24".to_owned()),
            mounts: Vec::new(),
        },
        image,
    }
}

/// Bytes of `path` that lie in data regions rather than holes.
fn allocated_bytes(path: &Path) -> u64 {
    let file = File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    data_extents(&file, size)
        .unwrap()
        .iter()
        .map(|(_, length)| length)
        .sum()
}

/// A 64 MiB disk with two extents far apart: the shape of a data disk.
fn sparse_disk(path: &Path) -> Vec<(u64, Vec<u8>)> {
    let extents = vec![
        (1 << 20, vec![0x11u8; 8192]),
        (40 << 20, vec![0x22u8; 300_000]),
    ];
    let file = File::create(path).unwrap();
    file.set_len(64 << 20).unwrap();
    for (offset, data) in &extents {
        file.write_all_at(data, *offset).unwrap();
    }
    extents
}

#[test]
fn a_sparse_disk_round_trips_without_its_holes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("data.img");
    let extents = sparse_disk(&disk);
    let archive = dir.path().join("dev.tar.zst");

    let size = write(&archive, &manifest(), &disk).unwrap();
    assert_eq!(size, archive.metadata().unwrap().len());
    // Only the extents are stored: far below the 64 MiB logical size.
    assert!(size < 1 << 20, "{size} bytes");
    assert!(!dir.path().join(".dev.tar.zst.tmp").exists());

    let read = read_manifest(&archive).unwrap();
    assert_eq!(read.machine.name, "dev");
    assert_eq!(read.image.name, "alpine-3.24-arm64");

    let restored = dir.path().join("restored.img");
    extract_data_disk(&archive, &restored).unwrap();
    assert_eq!(restored.metadata().unwrap().len(), 64 << 20);
    assert_eq!(
        std::fs::read(&restored).unwrap(),
        std::fs::read(&disk).unwrap()
    );
    // The holes came back as holes: APFS allocates in granules of a
    // couple of MiB around a write, so the data regions of the restored
    // disk cover the two extents and little else of its 64 MiB.
    let written: u64 = extents.iter().map(|(_, data)| data.len() as u64).sum();
    let data = allocated_bytes(&restored);
    assert!(
        data >= written && data < 8 << 20,
        "{data} bytes of data regions"
    );

    // The entry map is readable by what the format promises: the data
    // entry carries the GNU sparse 1.0 records and its real name.
    let mut tar = open(&archive).unwrap();
    let entries: Vec<_> = tar.entries().unwrap().map(|e| e.unwrap()).collect();
    assert_eq!(entries.len(), 2);
    let mut entries = entries;
    let (name, realsize) = sparse_pax_records(&mut entries[1]).unwrap().unwrap();
    assert_eq!(name, "data.img");
    assert_eq!(realsize, 64 << 20);
}

#[test]
fn write_refuses_to_replace_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("data.img");
    sparse_disk(&disk);
    let archive = dir.path().join("dev.tar.zst");
    std::fs::write(&archive, b"mine").unwrap();
    let err = write(&archive, &manifest(), &disk).unwrap_err();
    assert!(matches!(err, EngineError::Common(ref c) if c.is_already_exists()));
    assert_eq!(std::fs::read(&archive).unwrap(), b"mine");
}

#[test]
fn a_plain_data_entry_is_restored_sparse_too() {
    // An archive another tool wrote, with the disk as a regular entry.
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("plain.tar.zst");
    // 64 MiB: APFS keeps holes only in files larger than its allocation
    // granule, so a smaller disk would report full even when sparse.
    let mut dense = vec![0u8; 64 << 20];
    dense[20 << 20..(20 << 20) + 100].fill(0x33);
    {
        let encoder =
            zstd::stream::write::Encoder::new(File::create(&archive).unwrap(), 1).unwrap();
        let mut tar = tar::Builder::new(encoder);
        let json = serde_json::to_vec(&manifest()).unwrap();
        let mut header = regular_file_header(0o644, 0, json.len() as u64);
        tar.append_data(&mut header, MANIFEST_ENTRY, json.as_slice())
            .unwrap();
        let mut header = regular_file_header(0o600, 0, dense.len() as u64);
        tar.append_data(&mut header, DATA_DISK_ENTRY, dense.as_slice())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let restored = dir.path().join("restored.img");
    extract_data_disk(&archive, &restored).unwrap();
    assert_eq!(std::fs::read(&restored).unwrap(), dense);
    let data = allocated_bytes(&restored);
    assert!(data < 8 << 20, "{data} bytes of data regions");
}

#[test]
fn other_files_are_named_as_not_an_archive() {
    let dir = tempfile::tempdir().unwrap();
    let not_zstd = dir.path().join("notes.txt");
    std::fs::write(&not_zstd, b"hello").unwrap();
    assert!(read_manifest(&not_zstd).is_err());

    // A zstd tar whose first entry is something else.
    let other = dir.path().join("other.tar.zst");
    {
        let encoder = zstd::stream::write::Encoder::new(File::create(&other).unwrap(), 1).unwrap();
        let mut tar = tar::Builder::new(encoder);
        let mut header = regular_file_header(0o644, 0, 2);
        tar.append_data(&mut header, "README", b"hi".as_slice())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let err = read_manifest(&other).unwrap_err().to_string();
    assert!(
        err.contains("not a machine archive") && err.contains("README"),
        "{err}"
    );
    assert!(extract_data_disk(&other, &dir.path().join("x.img")).is_err());
}
