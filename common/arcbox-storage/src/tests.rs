use std::cell::Cell;
use std::fs::{self, OpenOptions};
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

#[test]
fn a_missing_paired_member_is_not_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    fs::remove_file(&metadata).unwrap();
    assert!(prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).is_err());
    assert!(!metadata.exists());
}

#[test]
fn unknown_and_truncated_existing_images_remain_untouched() {
    for bytes in [
        vec![0_u8; IMAGE_SIZE as usize],
        b"unreadable superblock".to_vec(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (data, metadata) = pair(dir.path());
        fs::write(&data, &bytes).unwrap();
        assert!(prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).is_err());
        assert_eq!(fs::read(&data).unwrap(), bytes);
        assert!(!metadata.exists());
        assert!(!manifest_path(&data).exists());
    }
}

#[test]
fn replaced_image_requires_explicit_verified_rebind() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    let manifest = prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    let copy = dir.path().join("copy");
    fs::create_dir(&copy).unwrap();
    fs::copy(&data, copy.join("docker.img")).unwrap();
    fs::copy(&metadata, copy.join("docker-meta.img")).unwrap();
    assert!(manifest.verify_images(&copy).is_err());
    manifest
        .rebind_images(&copy)
        .unwrap()
        .verify_images(&copy)
        .unwrap();
    fs::write(copy.join("docker-meta.img"), b"foreign image").unwrap();
    assert!(manifest.rebind_images(&copy).is_err());
}

#[test]
fn data_filesystem_without_metadata_requires_verified_legacy_migration() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    let uuid = Uuid::new_v4();
    write_filesystem(&data, VolumeRole::Data, uuid);
    let manifest = prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    assert_eq!(manifest.layout, StorageLayout::LegacyMigration);
    assert_eq!(manifest.data.filesystem_uuid, uuid);
    let called = Cell::new(false);
    assert!(
        ensure_filesystem(
            &manifest_path(&data),
            VolumeRole::Metadata,
            &metadata,
            |_| {
                called.set(true);
                Ok(())
            }
        )
        .is_err()
    );
    assert!(!called.get());
}

#[test]
fn corrupt_ready_signature_never_invokes_formatter() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    write_filesystem(&data, VolumeRole::Data, Uuid::new_v4());
    write_filesystem(&metadata, VolumeRole::Metadata, Uuid::new_v4());
    prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    let mut file = OpenOptions::new().write(true).open(&data).unwrap();
    file.seek(SeekFrom::Start(0x10040)).unwrap();
    file.write_all(&[0; 8]).unwrap();
    let called = Cell::new(false);
    assert!(
        ensure_filesystem(&manifest_path(&data), VolumeRole::Data, &data, |_| {
            called.set(true);
            Ok(())
        })
        .is_err()
    );
    assert!(!called.get());
}

#[test]
fn recognized_legacy_pair_does_not_assert_completed_metadata_migration() {
    let dir = tempfile::tempdir().unwrap();
    let (data, metadata) = pair(dir.path());
    write_filesystem(&data, VolumeRole::Data, Uuid::new_v4());
    write_filesystem(&metadata, VolumeRole::Metadata, Uuid::new_v4());
    let before = [fs::read(&data).unwrap(), fs::read(&metadata).unwrap()];
    let manifest = prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap();
    assert_eq!(manifest.layout, StorageLayout::LegacyPair);
    for (role, path) in [(VolumeRole::Data, &data), (VolumeRole::Metadata, &metadata)] {
        ensure_filesystem(&manifest_path(&data), role, path, |_| {
            panic!("legacy filesystem must not be reformatted")
        })
        .unwrap();
    }
    assert_eq!(
        [fs::read(&data).unwrap(), fs::read(&metadata).unwrap()],
        before
    );
    assert_eq!(
        prepare_pair(&data, &metadata, IMAGE_SIZE, IMAGE_SIZE).unwrap(),
        manifest
    );
}
