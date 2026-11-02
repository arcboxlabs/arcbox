//! Code-signature checks on an extracted QEMU build.

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
