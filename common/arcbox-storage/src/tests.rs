use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use uuid::Uuid;

use crate::*;

const IMAGE_SIZE: u64 = 128 * 1024;

fn pair(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        directory.join("docker.img"),
        directory.join("docker-meta.img"),
    )
}

fn write_filesystem(path: &Path, role: VolumeRole, uuid: Uuid) {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.set_len(IMAGE_SIZE).unwrap();
    let (magic_offset, magic, uuid_offset): (u64, &[u8], u64) = match role {
        VolumeRole::Data => (0x10040, b"_BHRfS_M", 0x10020),
        VolumeRole::Metadata => (0x438, &[0x53, 0xef], 0x468),
    };
    file.seek(SeekFrom::Start(magic_offset)).unwrap();
    file.write_all(magic).unwrap();
    file.seek(SeekFrom::Start(uuid_offset)).unwrap();
    file.write_all(uuid.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn new_volume_format_authority_is_durable_and_single_use() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    let initial = prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    let path = manifest_path(&data);
    let calls = Cell::new(0);
    ensure_filesystem(&path, VolumeRole::Data, &data, |uuid| {
        assert_eq!(
            StorageManifest::load(&path).unwrap().data.state,
            VolumeState::Formatting
        );
        calls.set(calls.get() + 1);
        write_filesystem(&data, VolumeRole::Data, uuid);
        Ok(())
    })
    .unwrap();
    ensure_filesystem(&path, VolumeRole::Data, &data, |_| {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    let manifest = StorageManifest::load(&path).unwrap();
    assert_eq!(manifest.data.filesystem_uuid, initial.data.filesystem_uuid);
    assert_eq!(manifest.data.state, VolumeState::Ready);
}

#[test]
fn failed_format_does_not_regain_authority_after_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    let path = manifest_path(&data);
    assert!(
        ensure_filesystem(&path, VolumeRole::Data, &data, |_| Err(
            Error::RecoveryRequired("formatter interrupted".into())
        ))
        .is_err()
    );
    assert_eq!(
        StorageManifest::load(&path).unwrap().data.state,
        VolumeState::Formatting
    );
    let called = Cell::new(false);
    assert!(
        ensure_filesystem(&path, VolumeRole::Data, &data, |_| {
            called.set(true);
            Ok(())
        })
        .is_err()
    );
    assert!(!called.get());
}

#[test]
fn completed_format_can_be_recognized_after_manifest_commit_was_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    let path = manifest_path(&data);
    assert!(
        ensure_filesystem(&path, VolumeRole::Data, &data, |uuid| {
            write_filesystem(&data, VolumeRole::Data, uuid);
            Err(Error::RecoveryRequired(
                "process interrupted after formatter exit".into(),
            ))
        })
        .is_err()
    );
    ensure_filesystem(&path, VolumeRole::Data, &data, |_| {
        panic!("must not format again")
    })
    .unwrap();
    assert_eq!(
        StorageManifest::load(&path).unwrap().data.state,
        VolumeState::Ready
    );
}
