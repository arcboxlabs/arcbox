//! System Keychain storage for validated public roots. Trust authorization stays in the login session.

use std::sync::{Mutex, MutexGuard};

use arcbox_helper::HelperError;
use arcbox_local_ca::PublicAuthority;
use security_framework::{
    certificate::SecCertificate,
    item::{
        AddRef, ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, Limit, Location,
        Reference, SearchResult,
    },
    os::macos::keychain::SecKeychain,
};
use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound};

const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";
// Serialize duplicate detection and certificate removal across helper requests.
static MUTATION: Mutex<()> = Mutex::new(());

pub fn install(ca: &PublicAuthority) -> Result<(), HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    let keychain = SecKeychain::open(SYSTEM_KEYCHAIN).map_err(HelperError::from)?;
    match ItemAddOptions::new(ItemAddValue::Ref(AddRef::Certificate(certificate.clone())))
        .set_location(Location::FileKeychain(keychain))
        .add()
    {
        Ok(()) => Ok(()),
        Err(error) if error.code() == errSecDuplicateItem && stored(&certificate)?.is_some() => {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn status(ca: &PublicAuthority) -> Result<bool, HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    Ok(stored(&certificate)?.is_some() && ca.admin_trusted().map_err(HelperError::from)?)
}

pub fn remove(ca: &PublicAuthority) -> Result<(), HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    if ca.has_admin_trust().map_err(HelperError::from)? {
        return Err(HelperError::other(
            "revoke this CA's Admin trust from the login session before removing the certificate",
        ));
    }
    if let Some(stored) = stored(&certificate)? {
        stored.delete().map_err(HelperError::from)?;
    }
    Ok(())
}

fn lock() -> Result<MutexGuard<'static, ()>, HelperError> {
    if super::super::fs_root::test_root().is_some() {
        return Err(HelperError::other(
            "CA trust is unavailable with a filesystem test root",
        ));
    }
    MUTATION
        .lock()
        .map_err(|_| HelperError::other("CA trust mutation lock poisoned"))
}

fn stored(certificate: &SecCertificate) -> Result<Option<SecCertificate>, HelperError> {
    let keychain = SecKeychain::open(SYSTEM_KEYCHAIN).map_err(HelperError::from)?;
    let items = ItemSearchOptions::new()
        .keychains(&[keychain])
        .class(ItemClass::certificate())
        .serial_number(
            &certificate
                .serial_number_bytes()
                .map_err(HelperError::from)?,
        )
        .limit(Limit::All)
        .load_refs(true)
        .search();
    let items = match items {
        Ok(items) => items,
        Err(error) if error.code() == errSecItemNotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for item in items {
        let SearchResult::Ref(Reference::Certificate(stored)) = item else {
            return Err(HelperError::other(
                "expected a System Keychain certificate reference",
            ));
        };
        if stored.to_der() == certificate.to_der() {
            return Ok(Some(stored));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, GeneralSubtree,
        IsCa, KeyPair, KeyUsagePurpose, NameConstraints,
    };

    fn authority() -> PublicAuthority {
        let mut parameters = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(
            DnType::CommonName,
            arcbox_constants::dns::LOCAL_CA_COMMON_NAME,
        );
        name.push(DnType::OrganizationName, "ArcBox");
        parameters.distinguished_name = name;
        parameters.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        parameters.name_constraints = Some(NameConstraints {
            permitted_subtrees: vec![GeneralSubtree::DnsName(
                arcbox_constants::dns::LOCAL_DOMAIN.into(),
            )],
            excluded_subtrees: vec![
                GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
                GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
            ],
        });
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
    #[ignore = "requires root; stores and removes two isolated public CAs in the System Keychain"]
    fn native_storage_lifecycle_preserves_other_roots() {
        // SAFETY: geteuid has no preconditions or side effects.
        assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
        struct Cleanup(PublicAuthority);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                remove(&self.0).expect("remove isolated test CA");
            }
        }
        let first = Cleanup(authority());
        let second = Cleanup(authority());
        let is_stored = |ca: &PublicAuthority| {
            stored(&SecCertificate::from_der(ca.der()).unwrap())
                .unwrap()
                .is_some()
        };
        assert!(!is_stored(&first.0));
        assert!(!status(&first.0).unwrap());
        install(&first.0).unwrap();
        install(&first.0).unwrap();
        install(&second.0).unwrap();
        assert!(is_stored(&first.0));
        assert!(is_stored(&second.0));
        assert!(!status(&first.0).unwrap());
        assert!(!status(&second.0).unwrap());
        assert!(!first.0.has_admin_trust().unwrap());
        assert!(!second.0.has_admin_trust().unwrap());
        remove(&first.0).unwrap();
        remove(&first.0).unwrap();
        assert!(!status(&first.0).unwrap());
        assert!(!is_stored(&first.0));
        assert!(is_stored(&second.0));
    }
}
