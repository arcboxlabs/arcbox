//! Publish only ArcBox-owned input to a distribution trust updater.

use std::{fs, path::Path};

use crate::{Error, PublicAuthority};

/// Atomically installs an owned source anchor, or durably removes that anchor.
///
/// The caller must serialize writes and refresh the distribution trust bundle.
/// Existing anchors must contain only the validated ArcBox section.
///
/// # Errors
///
/// Rejects inactive incoming roots, symlinks, foreign content, and I/O failures.
pub fn install_trust_anchor(path: &Path, authority: Option<&PublicAuthority>) -> Result<(), Error> {
    if authority.is_some_and(|root| !root.validity().is_valid_at(time::OffsetDateTime::now_utc())) {
        return Err(Error::InvalidAuthority(
            "root certificate is not currently valid",
        ));
    }
    let exists = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => {
            return Err(Error::InvalidTrustBundle(
                "source anchor is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(Error::io(path, error)),
    };
    if exists {
        let current = fs::read(path).map_err(|error| Error::io(path, error))?;
        if !super::reconcile_trust_bundle(&current, None)?.is_empty() {
            return Err(Error::InvalidTrustBundle(
                "source anchor contains unowned content",
            ));
        }
    }
    if authority.is_some() {
        arcbox_atomic_file::write_public(path, &super::reconcile_trust_bundle(&[], authority)?)?;
    } else {
        if exists {
            fs::remove_file(path).map_err(|error| Error::io(path, error))?;
        }
        let parent = path
            .parent()
            .ok_or(Error::InvalidTrustBundle("source anchor has no parent"))?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| Error::io(parent, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::KeyPair;
    use std::os::unix::fs::symlink;

    fn root() -> PublicAuthority {
        PublicAuthority::try_from(
            crate::authority::parameters()
                .self_signed(&KeyPair::generate().unwrap())
                .unwrap()
                .der()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn replacement_and_removal_replay_without_changing_neighbor_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arcbox.crt");
        let other = dir.path().join("user.crt");
        fs::write(&other, b"user root").unwrap();
        for current in [root(), root()] {
            install_trust_anchor(&path, Some(&current)).unwrap();
            let expected = super::super::reconcile_trust_bundle(&[], Some(&current)).unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected);
            install_trust_anchor(&path, Some(&current)).unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected);
        }
        install_trust_anchor(&path, None).unwrap();
        install_trust_anchor(&path, None).unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(other).unwrap(), b"user root");
    }

    #[test]
    fn foreign_content_and_symlinks_are_never_replaced_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arcbox.crt");
        let root = root();
        for content in [
            b"foreign root".to_vec(),
            [
                super::super::reconcile_trust_bundle(&[], Some(&root)).unwrap(),
                b"user addition".to_vec(),
            ]
            .concat(),
        ] {
            fs::write(&path, &content).unwrap();
            for desired in [Some(&root), None] {
                assert!(install_trust_anchor(&path, desired).is_err());
                assert_eq!(fs::read(&path).unwrap(), content);
            }
        }
        fs::remove_file(&path).unwrap();
        let target = dir.path().join("user.crt");
        fs::write(
            &target,
            super::super::reconcile_trust_bundle(&[], Some(&root)).unwrap(),
        )
        .unwrap();
        symlink(&target, &path).unwrap();
        for desired in [Some(&root), None] {
            assert!(install_trust_anchor(&path, desired).is_err());
            assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
        }
    }

    #[test]
    fn inactive_incoming_roots_leave_the_previous_anchor_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arcbox.crt");
        install_trust_anchor(&path, Some(&root())).unwrap();
        let original = fs::read(&path).unwrap();
        for shift in [time::Duration::days(30), time::Duration::days(-4000)] {
            let mut parameters = crate::authority::parameters();
            parameters.not_before += shift;
            parameters.not_after += shift;
            let invalid = PublicAuthority::try_from(
                parameters
                    .self_signed(&KeyPair::generate().unwrap())
                    .unwrap()
                    .der()
                    .to_vec(),
            )
            .unwrap();
            assert!(matches!(
                install_trust_anchor(&path, Some(&invalid)),
                Err(Error::InvalidAuthority(_))
            ));
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }
}
