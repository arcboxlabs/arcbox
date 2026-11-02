//! The QEMU build that runs the System VM on a Mac without hardware
//! virtualization.
//!
//! QEMU is not bundled. `assets.lock` pins one build (`[qemu]`), compiled into
//! the daemon like the boot assets, and that build is installed on demand
//! into `<data_dir>/runtime/qemu/<version>/`. The archive's SHA-256 must
//! match the pin and every binary in it must carry ArcBox's Developer ID
//! signature before the directory is published.
//!
//! A development daemon may name an unpublished build instead
//! ([`arcbox_constants::env::QEMU_VERSION`] and
//! [`arcbox_constants::env::QEMU_SHA256`]). CI builds are ad-hoc signed, so
//! that override accepts any intact signature whose executable identifier is
//! still ArcBox's ([`SignaturePolicy::Development`]); a pinned build never
//! does.

mod lockfile;
mod signature;
#[cfg(test)]
mod tests;

use arcbox_constants::env;

use crate::error::{ImageError, Result};

pub use signature::{QEMU_IDENTIFIER, SignaturePolicy};

/// One QEMU build: where its archive is and what it must contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuArtifact {
    /// Build version, e.g. `0.1.0`.
    pub version: String,
    /// Host architecture in asset names (`arm64`).
    pub arch: String,
    /// SHA-256 of the archive, lowercase hex.
    pub sha256: String,
    /// Base URL; the archive is `<cdn>/v<version>/<archive name>`.
    pub cdn: String,
    /// Signatures the extracted binaries must carry.
    pub signature: SignaturePolicy,
}

impl QemuArtifact {
    /// The build this daemon runs on `arch`: the development override when
    /// `development` and the override is set, else the `assets.lock` pin.
    /// `None` when neither names one, as before a build is published or on
    /// an architecture with none.
    ///
    /// # Errors
    /// Returns a configuration error for an incomplete or malformed override.
    pub fn select(arch: &str, development: bool) -> Result<Option<Self>> {
        select(arch, development, lockfile::pin(), |key| {
            std::env::var(key).ok()
        })
    }

    /// File name of the archive, e.g. `qemu-darwin-arm64-v0.1.0.tar.gz`.
    #[must_use]
    pub fn archive_name(&self) -> String {
        format!("qemu-darwin-{}-v{}.tar.gz", self.arch, self.version)
    }

    /// Where the archive is published.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "{}/v{}/{}",
            self.cdn.trim_end_matches('/'),
            self.version,
            self.archive_name()
        )
    }
}

fn select(
    arch: &str,
    development: bool,
    pin: Option<&lockfile::QemuPin>,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<QemuArtifact>> {
    let (version, sha256) = (var(env::QEMU_VERSION), var(env::QEMU_SHA256));
    if version.is_some() || sha256.is_some() {
        if development {
            let (Some(version), Some(sha256)) = (version, sha256) else {
                return Err(ImageError::config(format!(
                    "{} and {} must be set together",
                    env::QEMU_VERSION,
                    env::QEMU_SHA256
                )));
            };
            let sha256 = sha256.to_ascii_lowercase();
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ImageError::config(format!(
                    "{} is not a SHA-256 digest",
                    env::QEMU_SHA256
                )));
            }
            let cdn = var(env::QEMU_CDN)
                .or_else(|| pin.map(|pin| pin.cdn.clone()))
                .ok_or_else(|| ImageError::config(format!("{} is not set", env::QEMU_CDN)))?;
            return Ok(Some(QemuArtifact {
                version,
                arch: arch.to_owned(),
                sha256,
                cdn,
                signature: SignaturePolicy::Development,
            }));
        }
        tracing::warn!(
            "ignoring {} outside the development profile; running the pinned QEMU build",
            env::QEMU_VERSION
        );
    }
    Ok(pin.and_then(|pin| {
        let sha256 = &pin.arch.get(arch)?.sha256;
        (!sha256.is_empty()).then(|| QemuArtifact {
            version: pin.version.clone(),
            arch: arch.to_owned(),
            sha256: sha256.clone(),
            cdn: pin.cdn.clone(),
            signature: SignaturePolicy::DeveloperId,
        })
    }))
}
