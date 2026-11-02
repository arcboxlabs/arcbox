//! The `[qemu]` pin in `assets.lock`, embedded at compile time like `[boot]`.

use std::collections::HashMap;
use std::sync::LazyLock;

const LOCK_TOML: &str = include_str!("../../../../assets.lock");

#[derive(Debug, serde::Deserialize)]
struct AssetsLock {
    qemu: Option<QemuPin>,
}

/// The QEMU build this daemon runs when the System VM is emulated.
#[derive(Debug, serde::Deserialize)]
pub(super) struct QemuPin {
    pub(super) version: String,
    pub(super) cdn: String,
    #[serde(default)]
    pub(super) arch: HashMap<String, ArchPin>,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct ArchPin {
    /// SHA-256 of the archive. Empty until the build is published.
    pub(super) sha256: String,
}

static LOCK: LazyLock<AssetsLock> =
    LazyLock::new(|| toml::from_str(LOCK_TOML).expect("invalid assets.lock"));

pub(super) fn pin() -> Option<&'static QemuPin> {
    LOCK.qemu.as_ref()
}
