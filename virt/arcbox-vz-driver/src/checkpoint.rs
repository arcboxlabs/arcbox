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

use std::ffi::CStr;
use std::path::{Path, PathBuf};

use arcbox_vm_driver::{Error, Result, VmSpec};
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
