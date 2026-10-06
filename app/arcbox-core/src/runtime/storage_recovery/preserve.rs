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
