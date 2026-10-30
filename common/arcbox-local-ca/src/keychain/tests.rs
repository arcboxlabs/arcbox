use super::*;

mod rotation;
mod signing_identity;

#[test]
fn identity_lookup_rejects_multiple_matches() {
    let certificate = authority::parameters()
        .self_signed(&rcgen::KeyPair::generate().unwrap())
        .unwrap();
    let reference = || {
        SearchResult::Ref(Reference::Certificate(
            SecCertificate::from_der(certificate.der()).unwrap(),
        ))
    };
    assert!(unique_item(Vec::new()).unwrap().is_none());
    assert!(unique_item(vec![reference()]).unwrap().is_some());
    assert!(unique_item(vec![reference(), reference()]).is_err());
}

#[test]
#[ignore = "creates and deletes an isolated CA in the user's macOS Keychain"]
fn orphaned_certificate_never_creates_a_replacement_key() {
    let dir = tempfile::tempdir().unwrap();
    struct Cleanup<'a>(&'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            KeychainAuthority::delete(self.0).expect("remove isolated CA");
        }
    }
    let _cleanup = Cleanup(dir.path());
    let original = KeychainAuthority::open(dir.path()).unwrap();
    let root = original.root(Slot::Primary);
    let der = root.der.clone();
    drop(original);
    let label = KeychainAuthority::label(dir.path()).unwrap();
    find_key(&label).unwrap().unwrap().delete().unwrap();
    Journal::remove(dir.path()).unwrap();

    let open_rejected = KeychainAuthority::open(dir.path()).is_err();
    let private_key = find_key(&label);
    let owned = KeychainAuthority::owned_certificates(dir.path());
    let journal = Journal::load(dir.path());
    Journal::Stable { current: root }
        .persist(dir.path())
        .unwrap();
    assert!(open_rejected);
    assert!(private_key.unwrap().is_none());
    assert_eq!(owned.unwrap()[0].der(), der);
    assert!(journal.unwrap().is_none());
}

struct Profile(tempfile::TempDir);

impl Profile {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        KeychainAuthority::delete(self.path()).expect("remove isolated profile identities");
    }
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn journal_selects_current_root_and_missing_journal_cannot_recreate_primary() {
    let profile = Profile::new();
    let dir = profile.path();
    let primary = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
    let replacement = KeychainAuthority::open_slot(dir, Slot::Replacement, true)
        .unwrap()
        .root(Slot::Replacement);
    Journal::Prepared {
        current: primary.clone(),
        candidate: replacement.clone(),
    }
    .persist(dir)
    .unwrap();
    assert_eq!(
        KeychainAuthority::open(dir).unwrap().certificate.to_der(),
        primary.der
    );
    assert_eq!(
        KeychainAuthority::existing_certificate(dir).unwrap(),
        Some(primary.der.clone())
    );
    assert_eq!(KeychainAuthority::owned_certificates(dir).unwrap().len(), 2);

    let committing = Journal::Committing {
        previous: primary.clone(),
        current: replacement.clone(),
    };
    committing.persist(dir).unwrap();
    assert_eq!(
        KeychainAuthority::open(dir).unwrap().certificate.to_der(),
        replacement.der
    );
    assert_eq!(
        KeychainAuthority::existing_certificate(dir).unwrap(),
        Some(replacement.der)
    );
    KeychainAuthority::delete_root(dir, &primary).unwrap();
    Journal::remove(dir).unwrap();
    let open_rejected = KeychainAuthority::open(dir).is_err();
    let inspect_rejected = KeychainAuthority::existing_certificate(dir).is_err();
    let primary_items = KeychainAuthority::slot_has_keys(dir, Slot::Primary);
    committing.persist(dir).unwrap();
    assert!(open_rejected);
    assert!(inspect_rejected);
    assert!(!primary_items.unwrap());
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn public_inspection_rejects_a_different_recorded_root() {
    let profile = Profile::new();
    let other = Profile::new();
    let original = KeychainAuthority::open(profile.path())
        .unwrap()
        .root(Slot::Primary);
    let foreign = KeychainAuthority::open(other.path())
        .unwrap()
        .root(Slot::Primary);
    Journal::Stable { current: foreign }
        .persist(profile.path())
        .unwrap();
    let result = KeychainAuthority::existing_certificate(profile.path());
    Journal::Stable { current: original }
        .persist(profile.path())
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::InvalidAuthority(
            "Keychain CA differs from the recorded certificate"
        ))
    ));
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn recorded_identity_loss_preserves_der_without_regenerating_items() {
    for remove_private in [true, false] {
        let profile = Profile::new();
        let dir = profile.path();
        let root = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
        let label = KeychainAuthority::label(dir).unwrap();
        if remove_private {
            find_key(&label).unwrap().unwrap().delete().unwrap();
        } else {
            crate::migration::delete_certificate_der(&root.der).unwrap();
        }
        assert!(KeychainAuthority::open(dir).is_err());
        assert!(KeychainAuthority::existing_certificate(dir).is_err());
        assert_eq!(
            KeychainAuthority::owned_certificates(dir).unwrap()[0].der(),
            root.der
        );
        if remove_private {
            assert!(find_key(&label).unwrap().is_none());
        } else {
            let private = find_key(&label).unwrap().unwrap();
            assert!(certificate_for_key(&private).unwrap().is_none());
        }
    }
}

#[test]
#[ignore = "creates and deletes isolated identities in the user's macOS Keychain"]
fn cleanup_resumes_with_only_the_retired_public_key_remaining() {
    let profile = Profile::new();
    let dir = profile.path();
    let previous = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
    let current = KeychainAuthority::open_slot(dir, Slot::Replacement, true)
        .unwrap()
        .root(Slot::Replacement);
    let key = find_key(&KeychainAuthority::slot_label(dir, Slot::Replacement).unwrap())
        .unwrap()
        .unwrap();
    let candidate_hash = public_key_hash(&key).unwrap();
    let candidate_public = public_key_bytes(&key).unwrap();
    Journal::Committing {
        previous: previous.clone(),
        current,
    }
    .persist(dir)
    .unwrap();
    crate::migration::delete_certificate_der(&previous.der).unwrap();
    let label = KeychainAuthority::label(dir).unwrap();
    find_key(&label).unwrap().unwrap().delete().unwrap();
    assert!(
        find_key_class(&label, KeyClass::public())
            .unwrap()
            .is_some()
    );

    KeychainAuthority::delete(dir).unwrap();
    assert!(!KeychainAuthority::slot_has_keys(dir, Slot::Primary).unwrap());
    assert!(!KeychainAuthority::slot_has_keys(dir, Slot::Replacement).unwrap());
    assert!(
        certificate_by_public_key(&candidate_hash, &candidate_public)
            .unwrap()
            .is_none()
    );
    assert!(Journal::load(dir).unwrap().is_none());

    let current = KeychainAuthority::open(dir).unwrap().root(Slot::Primary);
    assert_ne!(current.der, previous.der);
    Journal::Preparing {
        current,
        target_slot: Slot::Replacement,
    }
    .persist(dir)
    .unwrap();
    let mut options = GenerateKeyOptions::default();
    options
        .set_key_type(KeyType::ec())
        .set_size_in_bits(256)
        .set_label(KeychainAuthority::slot_label(dir, Slot::Replacement).unwrap())
        .set_location(Location::DefaultFileKeychain);
    SecKey::new(&options).unwrap();
    KeychainAuthority::delete(dir).unwrap();
    assert!(!KeychainAuthority::slot_has_keys(dir, Slot::Primary).unwrap());
    assert!(!KeychainAuthority::slot_has_keys(dir, Slot::Replacement).unwrap());
}
