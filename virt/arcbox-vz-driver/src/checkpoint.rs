//! The `vz/v1` checkpoint image: what a restore needs besides VZ's state
//! file, and how a [`RestoreSpec`] lands on it.
//!
//! ```text
//! <dir>/state.vzvmsave   VZ's saved machine state (guest RAM, devices)
//! <dir>/disks/<id>.img   each writable disk, cloned while the VM was paused
//! <dir>/checkpoint.json  the manifest below; written last
//! ```
//!
//! VZ restores only into a configuration that matches the saving one,
//! machine identifier included, so the manifest records the whole spec
//! and the identifier. The state file is bound to a Secure Enclave key of
//! this Mac and user, and the framework versions its contents, so the
//! manifest also records the macOS build and a restore on another build is
//! refused before VZ is asked (design D6). Disk contents are not part of
//! VZ's state; the clones under `disks/` are the copy that matches it. A
//! restore attaches the disks [`RestoreSpec::disks`] names, so a caller
//! clones those files (or its own copies taken while the VM was held) for
//! each restore — one image restores into any number of clones.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

use arcbox_vm_driver::{
    CheckpointFormat, CheckpointImage, CheckpointKind, Error, RestoreSpec, Result, VmSpec,
};
use arcbox_vz::SessionState;
use serde::{Deserialize, Serialize};

use crate::{NAME, driver_error};

/// The format this driver writes and restores.
pub const FORMAT: &str = "vz/v1";
/// VZ's state file inside an image.
pub const STATE_FILE: &str = "state.vzvmsave";
/// The manifest inside an image.
const MANIFEST: &str = "checkpoint.json";
/// The directory of disk clones inside an image.
const DISKS: &str = "disks";

/// What a restore needs to rebuild the configuration that saved the state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`FORMAT`].
    pub format: String,
    /// `kern.osversion` of the Mac that wrote the image, e.g. `25F71`.
    pub os_build: String,
    /// The platform's machine identifier, hex encoded.
    pub machine_identifier: String,
    /// The spec the saving VM ran.
    pub spec: VmSpec,
}

impl Manifest {
    /// The manifest for a VM running `spec` on this Mac.
    pub fn new(spec: &VmSpec, machine_identifier: &[u8]) -> Result<Self> {
        Ok(Self {
            format: FORMAT.to_owned(),
            os_build: os_build()?,
            machine_identifier: hex(machine_identifier),
            spec: spec.clone(),
        })
    }

    /// Writes the manifest into `dir`, replacing any earlier one only once
    /// the new one is complete.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| driver_error("manifest", e))?;
        let tmp = dir.join(format!("{MANIFEST}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, dir.join(MANIFEST))?;
        Ok(())
    }

    /// Reads the manifest of `image`, refusing images this driver did not
    /// write and images from another macOS build.
    pub fn read(image: &CheckpointImage) -> Result<Self> {
        if image.format.as_str() != FORMAT {
            return Err(Error::ForeignCheckpoint(image.format.clone()));
        }
        if image.kind != CheckpointKind::Full {
            return Err(Error::InvalidSpec(format!(
                "{NAME}: diff checkpoints are not supported"
            )));
        }
        let path = image.dir.join(MANIFEST);
        let bytes = std::fs::read(&path)
            .map_err(|e| driver_error(format!("read {}", path.display()), e))?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|e| driver_error("manifest", e))?;
        if manifest.format != FORMAT {
            return Err(Error::ForeignCheckpoint(CheckpointFormat::new(
                manifest.format,
            )));
        }
        let here = os_build()?;
        if manifest.os_build != here {
            return Err(Error::PreconditionFailed(format!(
                "{NAME}: the checkpoint was written on macOS build {}, this Mac runs {here}; \
                 its memory cannot be restored, its disks can boot cold",
                manifest.os_build
            )));
        }
        Ok(manifest)
    }

    /// The identifier a restore must present.
    pub fn machine_identifier(&self) -> Result<Vec<u8>> {
        unhex(&self.machine_identifier)
            .ok_or_else(|| Error::InvalidSpec(format!("{NAME}: manifest machine identifier")))
    }

    /// The spec a restore under `restore` runs: the saved one with the new
    /// id, NICs and disk paths.
    ///
    /// Everything else is fixed by the state file. The NICs replace the
    /// saved ones one for one and keep their MACs (their attachments may
    /// change); every saved disk must be named, by id, and keeps its saved
    /// flags.
    pub fn restore_spec(&self, restore: RestoreSpec) -> Result<VmSpec> {
        let mut spec = self.spec.clone();
        if restore.nics.len() != spec.nics.len() {
            return Err(Error::InvalidSpec(format!(
                "{NAME}: the checkpoint has {} NICs, the restore names {}",
                spec.nics.len(),
                restore.nics.len()
            )));
        }
        // VZ refuses a restore whose NIC addresses differ from the saved
        // configuration with a bare "invalid argument": the guest's driver
        // read the MAC at boot. Clones keep it; each has its own datapath.
        for (saved, new) in spec.nics.iter().zip(&restore.nics) {
            if saved.mac != new.mac {
                return Err(Error::InvalidSpec(format!(
                    "{NAME}: NIC `{}` must keep the MAC {} it was checkpointed with, not {}",
                    saved.id, saved.mac, new.mac
                )));
            }
        }
        let mut paths: HashMap<String, PathBuf> = restore
            .disks
            .into_iter()
            .map(|disk| (disk.id, disk.path))
            .collect();
        for disk in &mut spec.disks {
            disk.path = paths.remove(&disk.id).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "{NAME}: the restore names no path for disk `{}`",
                    disk.id
                ))
            })?;
        }
        if let Some(extra) = paths.keys().next() {
            return Err(Error::InvalidSpec(format!(
                "{NAME}: the checkpoint has no disk `{extra}`"
            )));
        }
        spec.id = restore.id;
        spec.nics = restore.nics;
        spec.isolation = restore.isolation;
        spec.validate()?;
        Ok(spec)
    }
}

/// Where an image keeps its clone of the disk `id`, taken while the VM
/// was paused for the checkpoint. Only writable disks are cloned.
pub fn disk_clone(image_dir: &Path, id: &str) -> PathBuf {
    image_dir.join(DISKS).join(format!("{id}.img"))
}

/// Flushes and clones the writable disks of `spec` into `dir`. Runs while
/// the VM is paused, so the clones match the saved state; on APFS each is
/// a `clonefile`, which `std::fs::copy` tries first.
pub fn clone_disks(spec: &VmSpec, dir: &Path) -> Result<()> {
    let writable: Vec<_> = spec.disks.iter().filter(|d| !d.read_only).collect();
    if writable.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir.join(DISKS))?;
    for disk in writable {
        let context = || format!("clone disk `{}`", disk.id);
        std::fs::File::open(&disk.path)
            .and_then(|file| file.sync_all())
            .map_err(|e| driver_error(context(), e))?;
        let clone = disk_clone(dir, &disk.id);
        // A clone left by an earlier checkpoint into the same directory.
        match std::fs::remove_file(&clone) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(driver_error(context(), e)),
        }
        std::fs::copy(&disk.path, &clone).map_err(|e| driver_error(context(), e))?;
    }
    Ok(())
}

/// The error for a checkpoint the session cannot take now.
pub fn session_locked(state: SessionState) -> Error {
    Error::Unavailable(format!(
        "{NAME}: checkpoints need an unlocked console session; {state}"
    ))
}

/// This Mac's macOS build (`kern.osversion`), e.g. `25F71`.
pub fn os_build() -> Result<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated, `buf` holds `len` writable bytes
    // and `len` is updated in place; no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(driver_error(
            "kern.osversion",
            std::io::Error::last_os_error(),
        ));
    }
    let build =
        CStr::from_bytes_until_nul(&buf[..len]).map_err(|e| driver_error("kern.osversion", e))?;
    Ok(build.to_string_lossy().into_owned())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests;
