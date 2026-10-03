//! Cloning a machine: a copy-on-write clone of its data disk under a new
//! name, with its own hostname, DNS record and VM identity.
//!
//! The data disk is the only thing a machine owns on disk — the rootfs is a
//! shared read-only image — so a clone is one `clonefile(2)` call: instant,
//! and costing no space until the two machines diverge. The clone keeps the
//! source's kernel, command line, mounts, size and backend, with the
//! `arcbox.machine_name=` token rewritten to its own hostname, which the
//! guest applies on every boot (`machine_identity`), so nothing of the
//! source's name leaks through the overlay's `/etc/hostname`.
//!
//! Only a stopped machine can be cloned. A running machine's data disk is a
//! btrfs volume mounted read-write inside the guest, with dirty pages the
//! host cannot see and no quiesce path through the agent; a clone of it is
//! a crash-consistent image at best. The same rule covers `export`.

use std::path::{Path, PathBuf};

use arcbox_constants::cmdline::MACHINE_NAME_KEY;
use chrono::Utc;

use super::{MachineInfo, MachineManager, MachineState, reserve_hostname, vm_shared_dirs};
use crate::error::{EngineError, Result};
use crate::vm::{BlockDeviceConfig, VmConfig};

/// File name of a machine's data disk inside its directory.
pub(super) const DATA_DISK: &str = "data.img";

impl MachineManager {
    /// Clones machine `source` into a new machine `name`.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is taken or cannot be a hostname, `source`
    /// is unknown, is not a distro machine, or is not stopped, or if the
    /// data disk cannot be cloned.
    pub fn clone_machine(&self, source: &str, name: &str) -> Result<String> {
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let hostname = reserve_hostname(&machines, name)?;
        let src = machines
            .get(source)
            .ok_or_else(|| EngineError::not_found(source.to_owned()))?;
        let src_disk = stopped_data_disk(src)?;

        let machine_dir = self.machines_dir.join(name);
        std::fs::create_dir_all(&machine_dir)?;
        let disk = machine_dir.join(DATA_DISK);
        let built = clone_file(&src_disk, &disk).and_then(|()| {
            let info = cloned_record(src, name, &hostname, &src_disk, &disk);
            let vm_id = self.vm_manager.create(VmConfig {
                cpus: info.cpus,
                memory_mb: info.memory_mb,
                kernel: info.kernel.clone(),
                cmdline: info.cmdline.clone(),
                shared_dirs: vm_shared_dirs(&self.data_dir, &info.mounts),
                block_devices: info.block_devices.clone(),
                backend: info.backend,
                nested_virt: info.nested_virt,
                ..Default::default()
            })?;
            Ok(MachineInfo { vm_id, ..info })
        });
        let info = match built {
            Ok(info) => info,
            Err(e) => {
                // Nothing of the clone is registered yet; leave no half-made
                // directory behind to confuse the next attempt.
                if let Err(cleanup) = std::fs::remove_dir_all(&machine_dir) {
                    tracing::warn!(
                        machine = name,
                        error = %cleanup,
                        "could not remove the directory of a failed clone"
                    );
                }
                return Err(e);
            }
        };
        tracing::info!(machine = name, source, "cloned machine");
        self.register(&mut machines, info)
    }
}

/// The data disk of a stopped distro machine, the one thing a clone or an
/// export copies.
pub(super) fn stopped_data_disk(machine: &MachineInfo) -> Result<PathBuf> {
    let Some(disk) = machine
        .disk_path
        .clone()
        .filter(|_| machine.distro.is_some())
    else {
        return Err(EngineError::invalid_state(format!(
            "machine '{}' is not a distro machine with its own data disk",
            machine.name
        )));
    };
    if !matches!(machine.state, MachineState::Created | MachineState::Stopped) {
        let state = format!("{:?}", machine.state).to_lowercase();
        return Err(EngineError::invalid_state(format!(
            "machine '{}' is {state}; stop it first",
            machine.name
        )));
    }
    Ok(disk)
}

/// The record of a clone of `src`: everything but its identity — name,
/// hostname, data disk path — and the state a fresh machine has.
fn cloned_record(
    src: &MachineInfo,
    name: &str,
    hostname: &str,
    src_disk: &Path,
    disk: &Path,
) -> MachineInfo {
    let block_devices = src
        .block_devices
        .iter()
        .map(|device| {
            if Path::new(&device.path) == src_disk {
                BlockDeviceConfig {
                    path: disk.to_string_lossy().into_owned(),
                    read_only: device.read_only,
                }
            } else {
                device.clone()
            }
        })
        .collect();
    MachineInfo {
        name: name.to_owned(),
        state: MachineState::Created,
        vm_id: src.vm_id.clone(),
        cid: None,
        cpus: src.cpus,
        memory_mb: src.memory_mb,
        disk_gb: src.disk_gb,
        kernel: src.kernel.clone(),
        cmdline: src
            .cmdline
            .as_deref()
            .map(|cmdline| cmdline_with_hostname(cmdline, hostname)),
        block_devices,
        distro: src.distro.clone(),
        distro_version: src.distro_version.clone(),
        disk_path: Some(disk.to_path_buf()),
        ssh_key_path: None,
        ip_address: None,
        bridge_ip_address: None,
        backend: src.backend,
        nested_virt: src.nested_virt,
        created_at: Utc::now(),
        started_at: None,
        mounts: src.mounts.clone(),
    }
}

/// `cmdline` naming `hostname` as the machine's name: the existing
/// `arcbox.machine_name=` token is replaced, or one is appended when the
/// command line predates the token.
pub(super) fn cmdline_with_hostname(cmdline: &str, hostname: &str) -> String {
    let token = format!("{MACHINE_NAME_KEY}{hostname}");
    let mut replaced = false;
    let mut tokens: Vec<&str> = cmdline
        .split_whitespace()
        .map(|t| {
            if t.starts_with(MACHINE_NAME_KEY) {
                replaced = true;
                token.as_str()
            } else {
                t
            }
        })
        .collect();
    if !replaced {
        tokens.push(&token);
    }
    tokens.join(" ")
}

/// Clones `src` to `dst` without copying its data.
///
/// On APFS a `clonefile(2)` clone shares the source's blocks until either
/// side writes, so a sparse disk image stays sparse and the clone takes no
/// time and no space.
///
/// # Errors
///
/// Returns an error when `dst` exists, the two paths are on different
/// volumes, or the volume cannot clone files (not APFS).
#[cfg(target_os = "macos")]
pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let c_path = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| EngineError::config(format!("path {} contains NUL", path.display())))
    };
    let (c_src, c_dst) = (c_path(src)?, c_path(dst)?);
    // SAFETY: both arguments are valid NUL-terminated paths that outlive the
    // call; clonefile reads them and touches no other memory of ours.
    if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => EngineError::config(format!(
            "cannot clone {}: the data directory must be on an APFS volume ({err})",
            src.display()
        )),
        _ => EngineError::from(std::io::Error::new(
            err.kind(),
            format!("clonefile {} -> {}: {err}", src.display(), dst.display()),
        )),
    })
}

/// Copies `src` to `dst`. Off macOS there is no portable clone call; the
/// standard copy reflinks where the filesystem can (`copy_file_range` on
/// btrfs and XFS) and copies the data otherwise.
#[cfg(not(target_os = "macos"))]
pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(EngineError::already_exists(dst.display().to_string()));
    }
    std::fs::copy(src, dst)?;
    Ok(())
}
