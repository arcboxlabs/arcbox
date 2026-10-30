use super::*;

fn published(dir: &Path) -> Vec<u8> {
    let bytes = std::fs::read(dir.join(arcbox_constants::paths::guest::TLS_CA_CERT)).unwrap();
    PublicAuthority::from_pem(&bytes).unwrap().der().to_vec()
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn preparation_retries_reuse_the_candidate_and_untrusted_commit_preserves_publication() {
    let profile = Profile::new();
    let dir = profile.path();
    let _operation = crate::lock_profile(dir).unwrap();
    let authority = KeychainAuthority::open(dir).unwrap();
    authority.publish(dir).unwrap();
    let previous = authority.root(Slot::Primary);
    let old = previous.public().unwrap().fingerprint_sha256();
    drop(authority);
    let pair = KeychainAuthority::prepare_rotation(dir, &old).unwrap();
    let new = pair.candidate.fingerprint_sha256();
    let prepared = Journal::load(dir).unwrap();
    assert_ne!(pair.previous.der(), pair.candidate.der());
    for _ in 0..2 {
        let repeated = KeychainAuthority::prepare_rotation(dir, &old).unwrap();
        let pending = KeychainAuthority::pending_rotation(dir).unwrap().unwrap();
        assert_eq!(repeated.previous.der(), pair.previous.der());
        assert_eq!(repeated.candidate.der(), pair.candidate.der());
        assert_eq!(pending.previous.der(), pair.previous.der());
        assert_eq!(pending.candidate.der(), pair.candidate.der());
    }
    assert!(KeychainAuthority::prepare_rotation(dir, &new).is_err());
    for (expected_old, expected_new) in [
        ("stale", new.as_str()),
        (old.as_str(), "stale"),
        (&old, &new),
    ] {
        assert!(KeychainAuthority::commit_rotation(dir, expected_old, expected_new).is_err());
    }
    assert_eq!(Journal::load(dir).unwrap(), prepared);
    assert_eq!(published(dir), previous.der);
    assert_eq!(
        KeychainAuthority::existing_certificate(dir).unwrap(),
        Some(previous.der)
    );
    assert!(!pair.previous.has_trust().unwrap());
    assert!(!pair.candidate.has_trust().unwrap());
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn only_preparing_can_complete_a_candidate_private_key() {
    let profile = Profile::new();
    let dir = profile.path();
    let _operation = crate::lock_profile(dir).unwrap();
    let current = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
    let fingerprint = current.public().unwrap().fingerprint_sha256();
    let preparing = Journal::Preparing {
        current,
        target_slot: Slot::Replacement,
    };
    preparing.persist(dir).unwrap();
    let label = KeychainAuthority::slot_label(dir, Slot::Replacement).unwrap();
    let mut options = GenerateKeyOptions::default();
    options
        .set_key_type(KeyType::ec())
        .set_size_in_bits(256)
        .set_label(&label)
        .set_location(Location::DefaultFileKeychain);
    let key = SecKey::new(&options).unwrap();
    let public = public_key_bytes(&key).unwrap();
    assert!(certificate_for_key(&key).unwrap().is_none());
    assert!(KeychainAuthority::pending_rotation(dir).unwrap().is_none());
    assert_eq!(Journal::load(dir).unwrap(), Some(preparing));
    let pair = KeychainAuthority::prepare_rotation(dir, &fingerprint).unwrap();
    assert_eq!(
        public_key_bytes(&find_key(&label).unwrap().unwrap()).unwrap(),
        public
    );
    assert_eq!(
        certificate_for_key(&key).unwrap().unwrap().to_der(),
        pair.candidate.der()
    );
    assert!(matches!(
        Journal::load(dir).unwrap(),
        Some(Journal::Prepared { .. })
    ));
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn missing_prepared_or_committing_candidate_never_regenerates() {
    for committing in [false, true] {
        for remove_private in [false, true] {
            let profile = Profile::new();
            let dir = profile.path();
            let _operation = crate::lock_profile(dir).unwrap();
            let previous = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
            let old = previous.public().unwrap().fingerprint_sha256();
            let pair = KeychainAuthority::prepare_rotation(dir, &old).unwrap();
            let new = pair.candidate.fingerprint_sha256();
            if committing {
                Journal::Committing {
                    previous,
                    current: Root {
                        slot: Slot::Replacement,
                        der: pair.candidate.der().to_vec(),
                    },
                }
                .persist(dir)
                .unwrap();
            }
            let before = Journal::load(dir).unwrap();
            let label = KeychainAuthority::slot_label(dir, Slot::Replacement).unwrap();
            if remove_private {
                find_key(&label).unwrap().unwrap().delete().unwrap();
            } else {
                crate::migration::delete_certificate_der(pair.candidate.der()).unwrap();
            }
            assert!(KeychainAuthority::pending_rotation(dir).is_err());
            assert!(KeychainAuthority::prepare_rotation(dir, &old).is_err());
            assert!(KeychainAuthority::commit_rotation(dir, &old, &new).is_err());
            assert_eq!(Journal::load(dir).unwrap(), before);
            assert!(
                KeychainAuthority::owned_certificates(dir)
                    .unwrap()
                    .iter()
                    .any(|root| root.der() == pair.candidate.der())
            );
            if remove_private {
                assert!(find_key(&label).unwrap().is_none());
            } else {
                assert!(
                    certificate_for_key(&find_key(&label).unwrap().unwrap())
                        .unwrap()
                        .is_none()
                );
            }
        }
    }
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn committing_recheck_retains_previous_der_after_identity_cleanup() {
    let profile = Profile::new();
    let other = Profile::new();
    let untouched = KeychainAuthority::open(other.path())
        .unwrap()
        .root(Slot::Primary);
    let other_journal = Journal::load(other.path()).unwrap();
    let dir = profile.path();
    let _operation = crate::lock_profile(dir).unwrap();
    let authority = KeychainAuthority::open(dir).unwrap();
    authority.publish(dir).unwrap();
    let previous = authority.root(Slot::Primary);
    drop(authority);
    let old = previous.public().unwrap().fingerprint_sha256();
    let pair = KeychainAuthority::prepare_rotation(dir, &old).unwrap();
    let new = pair.candidate.fingerprint_sha256();
    let committing = Journal::Committing {
        previous: previous.clone(),
        current: Root {
            slot: Slot::Replacement,
            der: pair.candidate.der().to_vec(),
        },
    };
    committing.persist(dir).unwrap();
    KeychainAuthority::delete_root(dir, &previous).unwrap();
    let pending = KeychainAuthority::pending_rotation(dir).unwrap().unwrap();
    assert_eq!(pending.previous.der(), previous.der);
    assert_eq!(pending.candidate.der(), pair.candidate.der());
    assert!(KeychainAuthority::prepare_rotation(dir, &old).is_err());
    assert!(KeychainAuthority::commit_rotation(dir, &old, &new).is_err());
    assert!(!KeychainAuthority::slot_has_keys(dir, Slot::Primary).unwrap());
    assert_eq!(published(dir), previous.der);
    assert_eq!(Journal::load(dir).unwrap(), Some(committing));
    assert_eq!(
        KeychainAuthority::existing_certificate(other.path()).unwrap(),
        Some(untouched.der)
    );
    assert_eq!(Journal::load(other.path()).unwrap(), other_journal);
}

struct NativeTrustProfile {
    directory: std::path::PathBuf,
    roots: Vec<PublicAuthority>,
    cleaned: bool,
}

impl NativeTrustProfile {
    fn new() -> Self {
        Self {
            directory: tempfile::Builder::new()
                .prefix("arcbox-ca-rotation-")
                .tempdir()
                .unwrap()
                .keep(),
            roots: Vec::new(),
            cleaned: false,
        }
    }

    fn path(&self) -> &Path {
        &self.directory
    }

    fn remember(&mut self, root: &PublicAuthority) -> Result<(), crate::Error> {
        let filename = format!(
            "recovery-{}.der",
            root.fingerprint_sha256().replace(':', "")
        );
        arcbox_atomic_file::write(&self.directory.join(filename), root.der())?;
        self.roots
            .push(PublicAuthority::try_from(root.der().to_vec())?);
        Ok(())
    }

    fn revoke_trust(&self) -> Result<(), crate::Error> {
        for root in &self.roots {
            root.remove_admin_trust()?;
            crate::remove_user_trust_der(root.der())?;
            if root.has_trust()? {
                return Err(crate::Error::InvalidAuthority(
                    "isolated test root still has User or Admin trust after revocation",
                ));
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<(), crate::Error> {
        for root in &self.roots {
            if root.has_trust()? {
                return Err(crate::Error::InvalidAuthority(
                    "isolated test root regained trust before identity cleanup",
                ));
            }
        }
        KeychainAuthority::delete(&self.directory)?;
        std::fs::remove_dir_all(&self.directory)
            .map_err(|error| crate::Error::io(&self.directory, error))?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for NativeTrustProfile {
    fn drop(&mut self) {
        if !self.cleaned {
            // Unwinding must not request authorization again after the user cancels.
            eprintln!(
                "Native CA test cleanup is incomplete. Retained profile and recovery DER: {}",
                self.directory.display()
            );
            for root in &self.roots {
                eprintln!("Retained root SHA-256: {}", root.fingerprint_sha256());
            }
        }
    }
}

#[test]
fn native_trust_fixture_preserves_recovery_records_when_the_test_panics() {
    let mut profile = NativeTrustProfile::new();
    let path = profile.path().to_owned();
    let current = Root {
        slot: Slot::Primary,
        der: authority::parameters()
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap()
            .der()
            .to_vec(),
    };
    let root = current.public().unwrap();
    let fingerprint = root.fingerprint_sha256();
    profile.remember(&root).unwrap();
    let stable = Journal::Stable { current };
    stable.persist(&path).unwrap();
    let result = std::panic::catch_unwind(move || {
        let _profile = profile;
        panic!("simulated authorization cancellation");
    });
    assert!(result.is_err());
    assert_eq!(Journal::load(&path).unwrap(), Some(stable));
    assert_eq!(
        std::fs::read(path.join(format!("recovery-{}.der", fingerprint.replace(':', "")))).unwrap(),
        root.der()
    );
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
#[ignore = "requires a macOS login session; prompts to trust and revoke three isolated CAs"]
fn native_rotation_rejects_old_trust_and_resumes_publication_and_partial_cleanup() {
    let mut profile = NativeTrustProfile::new();
    let mut other = NativeTrustProfile::new();
    let dir = profile.path().to_owned();
    let _operation = crate::lock_profile(&dir).unwrap();
    let authority = KeychainAuthority::open(&dir).unwrap();
    authority.publish(&dir).unwrap();
    let previous = authority.root(Slot::Primary);
    drop(authority);
    profile.remember(&previous.public().unwrap()).unwrap();
    let other_root = KeychainAuthority::open(other.path())
        .unwrap()
        .root(Slot::Primary);
    let other_public = other_root.public().unwrap();
    other.remember(&other_public).unwrap();
    let other_journal = Journal::load(other.path()).unwrap();
    let old = previous.public().unwrap().fingerprint_sha256();
    let pair = KeychainAuthority::prepare_rotation(&dir, &old).unwrap();
    let new = pair.candidate.fingerprint_sha256();
    profile.remember(&pair.candidate).unwrap();
    pair.previous.trust_admin().unwrap();
    pair.candidate.trust_admin().unwrap();
    other_public.trust_admin().unwrap();
    let prepared = Journal::load(&dir).unwrap();
    assert!(KeychainAuthority::commit_rotation(&dir, &old, &new).is_err());
    assert_eq!(Journal::load(&dir).unwrap(), prepared);
    assert_eq!(published(&dir), previous.der);
    assert!(KeychainAuthority::load_root(&dir, &previous).is_ok());
    pair.previous.remove_admin_trust().unwrap();
    let candidate = Root {
        slot: Slot::Replacement,
        der: pair.candidate.der().to_vec(),
    };
    Journal::Committing {
        previous: previous.clone(),
        current: candidate.clone(),
    }
    .persist(&dir)
    .unwrap();
    assert_eq!(
        pair.candidate.publish(&dir).unwrap(),
        crate::Publication::Published
    );
    crate::migration::delete_certificate_der(&previous.der).unwrap();
    for _ in 0..2 {
        let committed = KeychainAuthority::commit_rotation(&dir, &old, &new).unwrap();
        assert_eq!(committed.root(Slot::Replacement), candidate);
    }
    let stable = Some(Journal::Stable { current: candidate });
    assert_eq!(Journal::load(&dir).unwrap(), stable);
    assert_eq!(published(&dir), pair.candidate.der());
    assert!(!KeychainAuthority::slot_has_keys(&dir, Slot::Primary).unwrap());
    assert!(KeychainAuthority::pending_rotation(&dir).unwrap().is_none());
    assert!(KeychainAuthority::prepare_rotation(&dir, &old).is_err());
    assert_eq!(Journal::load(&dir).unwrap(), stable);
    assert!(!pair.previous.has_trust().unwrap());
    assert!(pair.candidate.admin_trusted().unwrap());
    assert!(other_public.admin_trusted().unwrap());
    assert_eq!(
        KeychainAuthority::existing_certificate(other.path()).unwrap(),
        Some(other_root.der)
    );
    assert_eq!(Journal::load(other.path()).unwrap(), other_journal);
    let next = KeychainAuthority::prepare_rotation(&dir, &new).unwrap();
    profile.remember(&next.candidate).unwrap();
    assert_eq!(next.previous.der(), pair.candidate.der());
    assert_ne!(next.candidate.der(), pair.previous.der());
    assert!(matches!(
        Journal::load(&dir).unwrap(),
        Some(Journal::Prepared {
            candidate: Root {
                slot: Slot::Primary,
                ..
            },
            ..
        })
    ));
    profile.revoke_trust().unwrap();
    other.revoke_trust().unwrap();
    profile.cleanup().unwrap();
    other.cleanup().unwrap();
}
