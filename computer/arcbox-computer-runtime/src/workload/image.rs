//! `docker:<ref>` templates and their registry pull.

use oci2rootfs::Platform;

use super::WorkloadError;

/// Template prefix naming a container image.
pub const DOCKER_TEMPLATE_PREFIX: &str = "docker:";

/// Registry Docker's short names resolve against.
const DEFAULT_REGISTRY: &str = "docker.io";

/// A fully qualified image reference: `registry/repository[:tag][@digest]`.
///
/// Docker's short forms (`debian:12`, `org/app`) are expanded the way
/// `docker pull` expands them before anything parses them, because
/// `oci2rootfs` 0.2 takes the text before the first `:` of `debian:12` for
/// a registry host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef(String);

impl ImageRef {
    /// The image a `docker:<ref>` template names, or `None` for any other
    /// template.
    pub fn from_template(template: &str) -> Result<Option<Self>, WorkloadError> {
        template
            .trim()
            .strip_prefix(DOCKER_TEMPLATE_PREFIX)
            .map(Self::parse)
            .transpose()
    }

    /// Qualify a Docker image reference.
    pub fn parse(reference: &str) -> Result<Self, WorkloadError> {
        let invalid = || WorkloadError::Image(format!("invalid image reference {reference:?}"));
        if reference.is_empty() || reference.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        let (name, digest) = match reference.split_once('@') {
            Some((name, digest)) if !digest.is_empty() => (name, Some(digest)),
            Some(_) => return Err(invalid()),
            None => (reference, None),
        };
        // A tag follows the last `:` after the last `/`; a `:` before that
        // belongs to a registry port.
        let (name, tag) = match name.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo, Some(tag)),
            _ => (name, None),
        };
        if name.is_empty() || name.ends_with('/') || tag.is_some_and(str::is_empty) {
            return Err(invalid());
        }
        // The first component names a registry only when more follow and it
        // looks like a host, as in Docker's reference grammar.
        let (registry, repository) = match name.split_once('/') {
            Some((host, rest))
                if host.contains('.') || host.contains(':') || host == "localhost" =>
            {
                (host, rest.to_owned())
            }
            _ => (DEFAULT_REGISTRY, name.to_owned()),
        };
        let repository = if registry == DEFAULT_REGISTRY && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        let tag = match (tag, digest) {
            (Some(tag), _) => format!(":{tag}"),
            (None, None) => ":latest".to_owned(),
            (None, Some(_)) => String::new(),
        };
        let digest = digest
            .map(|digest| format!("@{digest}"))
            .unwrap_or_default();
        let qualified = format!("{registry}/{repository}{tag}{digest}");
        Ok(Self(qualified))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The platform a computer on this host runs: Linux on the host's CPU.
pub fn guest_platform() -> Platform {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    Platform::new("linux", arch)
}

/// An image pulled from its registry, ready for
/// [`RootfsSource::Image`](crate::RootfsSource::Image).
#[cfg(feature = "remote-image")]
pub struct PulledImage {
    /// The layers, held in memory until the rootfs build consumes them.
    pub source: oci2rootfs::ImageSource,
    /// The image's runtime config, for [`super::bundle_spec`]. Kept apart
    /// because building the rootfs consumes `source`.
    pub config: Option<oci2rootfs::ContainerConfig>,
    /// Digest of the platform manifest that was pulled.
    pub manifest_digest: String,
}

/// Pull `image` for [`guest_platform`] from its registry, with credentials
/// from the user's Docker config.
///
/// A registry that does not have the image fails here; exporting it from
/// the System VM's dockerd instead (ADR 0011 D5) is the composer's call.
#[cfg(feature = "remote-image")]
pub async fn pull(image: &ImageRef) -> Result<PulledImage, WorkloadError> {
    let source = oci2rootfs::RemoteRef::new(image.as_str())
        .platform(guest_platform())
        .fetch()
        .await
        .map_err(|error| WorkloadError::Image(format!("pull {image}: {error}")))?;
    let manifest_digest = source
        .manifest_digest()
        .map(ToString::to_string)
        .unwrap_or_default();
    let config = source.config().and_then(|config| config.config.clone());
    Ok(PulledImage {
        source,
        config,
        manifest_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_are_qualified_like_docker_pull() {
        for (reference, expected) in [
            ("debian", "docker.io/library/debian:latest"),
            // A numeric tag, which oci2rootfs 0.2 would take for a port.
            ("debian:12", "docker.io/library/debian:12"),
            ("org/app:v1", "docker.io/org/app:v1"),
            (
                "docker.io/library/alpine:3.20",
                "docker.io/library/alpine:3.20",
            ),
            ("ghcr.io/org/team/app", "ghcr.io/org/team/app:latest"),
            ("localhost:5000/app", "localhost:5000/app:latest"),
            ("localhost:5000/app:dev", "localhost:5000/app:dev"),
            ("localhost/app", "localhost/app:latest"),
            ("alpine@sha256:abc", "docker.io/library/alpine@sha256:abc"),
            ("ghcr.io/org/img:1@sha256:ab", "ghcr.io/org/img:1@sha256:ab"),
        ] {
            assert_eq!(
                ImageRef::parse(reference).unwrap().as_str(),
                expected,
                "{reference}"
            );
        }
    }

    #[test]
    fn malformed_references_fail() {
        for reference in ["", "a b", "app:", "app@", "ghcr.io/", "@sha256:ab"] {
            assert!(
                matches!(ImageRef::parse(reference), Err(WorkloadError::Image(_))),
                "{reference:?}"
            );
        }
    }

    #[test]
    fn only_docker_templates_name_an_image() {
        assert_eq!(
            ImageRef::from_template(" docker:alpine:3.20").unwrap(),
            Some(ImageRef("docker.io/library/alpine:3.20".into()))
        );
        assert_eq!(ImageRef::from_template("").unwrap(), None);
        assert_eq!(ImageRef::from_template("my-template").unwrap(), None);
        assert!(ImageRef::from_template("docker:").is_err());
    }

    #[test]
    fn the_guest_platform_is_linux_on_the_host_cpu() {
        let platform = guest_platform();
        assert_eq!(platform.os(), "linux");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(platform.arch(), "arm64");
        #[cfg(target_arch = "x86_64")]
        assert_eq!(platform.arch(), "amd64");
    }
}
