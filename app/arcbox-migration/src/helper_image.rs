//! Helper image constants for volume-copy containers.

/// Returns the helper image reference used for temporary migration containers.
#[must_use]
pub const fn helper_image_reference() -> &'static str {
    "arcbox-migration-helper:latest"
}
