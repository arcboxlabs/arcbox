//! Makes durable copy-on-write snapshots while the original VM is stopped.

use crate::error::{CoreError, Result};
use arcbox_engine::{machine::MachineInfo, vm::BlockDeviceConfig};
use std::{fs, path::Path};

pub(super) fn pair(machine: &MachineInfo, directory: &Path) -> Result<Vec<BlockDeviceConfig>> {
    let [root, data, metadata] = machine.block_devices.as_slice() else {
        return Err(CoreError::invalid_state(
            "recovery requires the rootfs, data image, and metadata image",
        ));
    };
    fs::create_dir(directory)?;
    let parent = directory
        .parent()
        .ok_or_else(|| CoreError::config("recovery directory has no parent"))?;
    fs::File::open(parent)?.sync_all()?;
    let mut devices = vec![BlockDeviceConfig {
        path: root.path.clone(),
        read_only: true,
    }];
    for source in [data, metadata] {
        let source = Path::new(&source.path);
        let name = source
            .file_name()
            .ok_or_else(|| CoreError::config("disk image has no filename"))?;
        let destination = directory.join(name);
        snapshot(source, &destination)?;
        fs::File::open(&destination)?.sync_all()?;
        devices.push(BlockDeviceConfig {
            path: destination.to_string_lossy().into_owned(),
            read_only: true,
        });
    }
    let manifest = arcbox_storage::manifest_path(Path::new(&data.path));
    // Retain the original manifest bytes even when filesystem corruption prevents validation.
    arcbox_atomic_file::write(
        &arcbox_storage::manifest_path(Path::new(&devices[1].path)),
        &fs::read(manifest)?,
    )?;
    fs::File::open(directory)?.sync_all()?;
    Ok(devices)
}

/// Clones the preserved data copy into `<recovery directory>/repair/` and
/// returns the devices the repair guest boots: the rootfs, that writable
/// clone, and the read-only preserved metadata copy. The preserved copies
/// stay untouched as the evidence of what the check saw.
pub(super) fn repair_copy(preserved: &[BlockDeviceConfig]) -> Result<Vec<BlockDeviceConfig>> {
    let [root, data, metadata] = preserved else {
        return Err(CoreError::invalid_state(
            "repair requires the preserved rootfs, data image, and metadata image",
        ));
    };
    let source = Path::new(&data.path);
    let recovery = source
        .parent()
        .ok_or_else(|| CoreError::config("preserved data image has no directory"))?;
    let directory = recovery.join("repair");
    fs::create_dir(&directory)?;
    fs::File::open(recovery)?.sync_all()?;
    let name = source
        .file_name()
        .ok_or_else(|| CoreError::config("disk image has no filename"))?;
    let copy = directory.join(name);
    snapshot(source, &copy)?;
    fs::File::open(&copy)?.sync_all()?;
    fs::File::open(&directory)?.sync_all()?;
    Ok(vec![
        root.clone(),
        BlockDeviceConfig {
            path: copy.to_string_lossy().into_owned(),
            read_only: false,
        },
        metadata.clone(),
    ])
}

/// Replaces the live data image with the repaired copy, atomically.
///
/// The image it replaces is preserved in the recovery directory, so a
/// failure after this point never loses the original.
pub(super) fn install_repaired(repaired: &Path, live: &Path) -> Result<()> {
    fs::rename(repaired, live)?;
    let parent = live
        .parent()
        .ok_or_else(|| CoreError::config("data image has no directory"))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    arcbox_engine::machine::clone_file(source, destination)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let source = fs::File::open(source)?;
    let copy = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // SAFETY: both file descriptors remain open for the synchronous FICLONE ioctl.
    let result = unsafe { libc::ioctl(copy.as_raw_fd(), 0x4004_9409, source.as_raw_fd()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &Path, read_only: bool) -> BlockDeviceConfig {
        BlockDeviceConfig {
            path: path.to_string_lossy().into_owned(),
            read_only,
        }
    }

    /// The repair writes only a clone of the preserved copy. The preserved
    /// copy keeps what the check saw, and the live image is replaced only by
    /// the explicit install, whose old contents the preserved copy still has.
    #[test]
    fn repair_works_on_a_clone_and_installs_by_rename() {
        let directory = tempfile::tempdir().unwrap();
        let live = directory.path().join("docker.img");
        let recovery = directory.path().join("recovery");
        fs::create_dir(&recovery).unwrap();
        let preserved_data = recovery.join("docker.img");
        let preserved_meta = recovery.join("docker-meta.img");
        fs::write(&live, b"original").unwrap();
        fs::write(&preserved_data, b"original").unwrap();
        fs::write(&preserved_meta, b"metadata").unwrap();
        let root = directory.path().join("rootfs.erofs");
        let preserved = [
            device(&root, true),
            device(&preserved_data, true),
            device(&preserved_meta, true),
        ];

        let devices = repair_copy(&preserved).unwrap();
        let copy = recovery.join("repair/docker.img");
        assert_eq!(devices[0].path, preserved[0].path);
        assert_eq!(devices[1].path, copy.to_string_lossy());
        assert!(
            !devices[1].read_only,
            "the repair guest must write the clone"
        );
        assert_eq!(devices[2].path, preserved[2].path);
        assert!(devices[2].read_only);
        assert!(
            repair_copy(&preserved).is_err(),
            "an existing repair is never reused"
        );

        fs::write(&copy, b"repaired").unwrap();
        assert_eq!(fs::read(&preserved_data).unwrap(), b"original");
        assert_eq!(fs::read(&live).unwrap(), b"original");
        install_repaired(&copy, &live).unwrap();
        assert_eq!(fs::read(&live).unwrap(), b"repaired");
        assert!(!copy.exists());
        assert_eq!(fs::read(&preserved_data).unwrap(), b"original");
    }
}
