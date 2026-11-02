//! Code-signature checks on an extracted QEMU build.

use std::path::Path;
use std::process::Command;

/// Signed with a Developer ID Application certificate issued to ArcBox, Inc.
const ARCBOX_DEVELOPER_ID: &str = "anchor apple generic \
    and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
    and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
    and certificate leaf[subject.OU] = \"422ACSY6Y5\"";

/// Signing identifier of the QEMU executable in every ArcBox build.
pub const QEMU_IDENTIFIER: &str = "com.arcboxlabs.qemu.system-aarch64";

/// Signatures the binaries of a QEMU build must carry before it is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Every binary is signed with ArcBox's Developer ID (Team 422ACSY6Y5),
    /// and the executable's identifier is [`QEMU_IDENTIFIER`]. Every build
    /// `assets.lock` pins is held to this.
    DeveloperId,
    /// Any intact signature, ad-hoc included, with the executable still
    /// signed as [`QEMU_IDENTIFIER`]. Only a development override of the pin
    /// selects it: CI builds are ad-hoc signed.
    Development,
}

impl SignaturePolicy {
    /// The code requirement a binary must satisfy, beyond an intact signature.
    pub(super) fn requirement(self, executable: bool) -> Option<String> {
        match (self, executable) {
            (Self::DeveloperId, true) => Some(format!(
                "{ARCBOX_DEVELOPER_ID} and identifier \"{QEMU_IDENTIFIER}\""
            )),
            (Self::DeveloperId, false) => Some(ARCBOX_DEVELOPER_ID.to_owned()),
            (Self::Development, true) => Some(format!("identifier \"{QEMU_IDENTIFIER}\"")),
            (Self::Development, false) => None,
        }
    }
}

/// Checks one binary's signature.
pub(super) trait Verify: Send + Sync {
    /// Fails with the reason unless `path` carries an intact signature that
    /// satisfies `requirement`, when one is given.
    fn verify(&self, path: &Path, requirement: Option<&str>) -> Result<(), String>;
}

/// `codesign --verify --strict`, with the requirement as `-R`.
pub(super) struct Codesign;

impl Verify for Codesign {
    fn verify(&self, path: &Path, requirement: Option<&str>) -> Result<(), String> {
        let mut command = Command::new("/usr/bin/codesign");
        command.args(["--verify", "--strict"]);
        if let Some(requirement) = requirement {
            command.arg(format!("-R={requirement}"));
        }
        let output = command
            .arg(path)
            .output()
            .map_err(|error| format!("could not run codesign: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        // codesign prefixes the path it checked; the caller names the file.
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.trim().replace(&format!("{}: ", path.display()), ""))
    }
}
