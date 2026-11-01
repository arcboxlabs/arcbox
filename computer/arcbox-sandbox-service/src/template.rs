//! Parse a sandbox template reference (CORE-54).
//!
//! The product API names *what runs* in a sandbox with one opaque string;
//! nothing in it is a host path. Local mode understands two forms:
//!
//! - `""` — the built-in minimal template (busybox + `vm-agent`), built on
//!   first use.
//! - `"docker:<ref>"` — a container image, which the composer turns into a
//!   rootfs ([`crate::SandboxHooks::image_rootfs`]).
//!
//! Anything else is a catalog reference (`templates.rs`); an unrecognised
//! form is rejected rather than guessed at.

use crate::error::SandboxError;

/// Prefix selecting a container image.
const DOCKER_PREFIX: &str = "docker:";

/// A template reference this service resolves without the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Template {
    /// Built-in busybox + `vm-agent` image.
    Default,
    /// A container image reference.
    DockerImage(String),
}

impl Template {
    /// Parse a wire template reference.
    ///
    /// Unknown forms are rejected: silently treating them as an image name
    /// would turn a typo into a confusing pull failure minutes later, and a
    /// future registry form must not be swallowed by this arm.
    pub fn parse(reference: &str) -> Result<Self, SandboxError> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Ok(Self::Default);
        }
        if let Some(image) = reference.strip_prefix(DOCKER_PREFIX) {
            if image.is_empty() {
                return Err(SandboxError::InvalidArgument(
                    "template 'docker:' is missing an image reference".into(),
                ));
            }
            return Ok(Self::DockerImage(image.to_owned()));
        }
        Err(SandboxError::InvalidArgument(format!(
            "unknown template {reference:?}; expected \"\" (built-in) or \"docker:<image>\""
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_built_in_and_docker_forms() {
        assert_eq!(Template::parse("").unwrap(), Template::Default);
        assert_eq!(Template::parse("   ").unwrap(), Template::Default);
        assert_eq!(
            Template::parse("docker:alpine:3.20").unwrap(),
            Template::DockerImage("alpine:3.20".into())
        );
    }

    #[test]
    fn parse_rejects_unknown_forms_instead_of_guessing() {
        // A bare image name is NOT accepted: it is the future registry-name
        // form, and guessing would make a typo surface much later.
        assert!(matches!(
            Template::parse("alpine"),
            Err(SandboxError::InvalidArgument(_))
        ));
        // A host path must never resolve (the whole point of CORE-54).
        assert!(matches!(
            Template::parse("/var/lib/arcbox/rootfs.ext4"),
            Err(SandboxError::InvalidArgument(_))
        ));
        assert!(matches!(
            Template::parse("docker:"),
            Err(SandboxError::InvalidArgument(_))
        ));
    }
}
