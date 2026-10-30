//! Persist an explicit ArcBox root in a caller-selected PEM trust bundle.

use std::path::Path;

use crate::{Error, PublicAuthority};

/// Installs or explicitly removes the ArcBox root in an existing trust bundle.
///
/// The caller selects the guest-owned path and serializes updates to that path.
/// Reads may follow a symlink; publication replaces that link, never its target.
///
/// # Errors
///
/// Rejects an inactive incoming root, an invalid owned section, and every file
/// read, publication, or durability failure. No success is returned before fsync.
pub fn install_trust_bundle(path: &Path, authority: Option<&PublicAuthority>) -> Result<(), Error> {
    if authority.is_some_and(|root| !root.validity().is_valid_at(time::OffsetDateTime::now_utc())) {
        return Err(Error::InvalidAuthority(
            "root certificate is not currently valid",
        ));
    }
    let current = std::fs::read(path).map_err(|source| Error::io(path, source))?;
    let replacement = super::reconcile_trust_bundle(&current, authority)?;
    // Replay must confirm durability even if an earlier uncertain write published these bytes.
    arcbox_atomic_file::write_public(path, &replacement)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use rcgen::KeyPair;
    use time::Duration;

    use super::*;

    fn root(shift: Duration) -> PublicAuthority {
        let mut parameters = crate::authority::parameters();
        parameters.not_before += shift;
        parameters.not_after += shift;
        PublicAuthority::try_from(
            parameters
                .self_signed(&KeyPair::generate().unwrap())
                .unwrap()
                .der()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn replaces_the_symlink_without_changing_the_baseline_and_removes_only_its_root() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("immutable-baseline.pem");
        let system = directory.path().join("system-bundle.pem");
        let other_roots = b"existing distro roots\n";
        fs::write(&baseline, other_roots).unwrap();
        symlink(&baseline, &system).unwrap();
        let current = root(Duration::ZERO);

        install_trust_bundle(&system, Some(&current)).unwrap();
        let installed = fs::read(&system).unwrap();
        assert!(fs::symlink_metadata(&system).unwrap().is_file());
        assert_eq!(fs::read(&baseline).unwrap(), other_roots);
        assert!(installed.starts_with(other_roots));
        assert_eq!(
            fs::metadata(&system).unwrap().permissions().mode() & 0o777,
            0o644
        );
        install_trust_bundle(&system, Some(&current)).unwrap();
        assert_eq!(fs::read(&system).unwrap(), installed);

        let added_root = b"user-added root\n";
        fs::write(&system, [installed.as_slice(), added_root].concat()).unwrap();
        install_trust_bundle(&system, None).unwrap();
        assert_eq!(
            fs::read(&system).unwrap(),
            [other_roots.as_slice(), added_root].concat()
        );
        assert_eq!(fs::read(&baseline).unwrap(), other_roots);
    }

    #[test]
    fn rejects_inactive_roots_before_touching_the_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system-bundle.pem");
        let initial = b"existing distro roots\n";
        fs::write(&system, initial).unwrap();
        for shift in [Duration::days(-4000), Duration::days(30)] {
            let invalid = root(shift);
            assert!(matches!(
                install_trust_bundle(&system, Some(&invalid)),
                Err(Error::InvalidAuthority(_))
            ));
            assert_eq!(fs::read(&system).unwrap(), initial);
        }
    }

    #[test]
    fn read_and_owned_section_failures_leave_existing_bytes_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system-bundle.pem");
        let current = root(Duration::ZERO);
        assert!(matches!(
            install_trust_bundle(&system, Some(&current)),
            Err(Error::Io { .. })
        ));
        assert!(!system.exists());
        let damaged = b"other roots\n# BEGIN ARCBOX LOCAL CA\nunverified certificate\n# END ARCBOX LOCAL CA\n";
        fs::write(&system, damaged).unwrap();
        for desired in [Some(&current), None] {
            assert!(install_trust_bundle(&system, desired).is_err());
            assert_eq!(fs::read(&system).unwrap(), damaged);
        }
    }
}
