//! TLS-only system trust for validated public ArcBox roots. No private keys enter the helper.

use std::sync::{Mutex, MutexGuard};

use arcbox_helper::HelperError;
use arcbox_local_ca::PublicAuthority;
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use security_framework::{
    certificate::SecCertificate,
    item::{
        AddRef, ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, Limit, Location,
        Reference, SearchResult,
    },
    os::macos::keychain::SecKeychain,
    policy::SecPolicy,
    secure_transport::SslProtocolSide,
};
use security_framework_sys::{
    base::{SecCertificateRef, errSecDuplicateItem, errSecItemNotFound, errSecNoTrustSettings},
    trust_settings::{
        SecTrustSettingsCopyTrustSettings, SecTrustSettingsSetTrustSettings,
        kSecTrustSettingsDomainAdmin, kSecTrustSettingsResultTrustRoot,
    },
};

const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";
const POLICY: &str = "kSecTrustSettingsPolicy";
const POLICY_NAME: &str = "kSecTrustSettingsPolicyName";
const RESULT: &str = "kSecTrustSettingsResult";
// The certificate and its trust settings are separate native mutations.
static MUTATION: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn SecTrustSettingsRemoveTrustSettings(certificate: SecCertificateRef, domain: u32) -> i32;
}

pub fn install(ca: &PublicAuthority) -> Result<(), HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    let keychain = SecKeychain::open(SYSTEM_KEYCHAIN).map_err(HelperError::from)?;
    match ItemAddOptions::new(ItemAddValue::Ref(AddRef::Certificate(certificate.clone())))
        .set_location(Location::FileKeychain(keychain))
        .add()
    {
        Ok(()) => {}
        Err(error) if error.code() == errSecDuplicateItem && stored(&certificate)?.is_some() => {}
        Err(error) => return Err(error.into()),
    }
    let settings = tls_settings();
    // SAFETY: the owned certificate and dictionary remain valid for the native call.
    check(unsafe {
        SecTrustSettingsSetTrustSettings(
            certificate.as_concrete_TypeRef(),
            kSecTrustSettingsDomainAdmin,
            settings.as_CFTypeRef(),
        )
    })
}

pub fn status(ca: &PublicAuthority) -> Result<bool, HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    Ok(stored(&certificate)?.is_some()
        && trust_settings(&certificate)?.is_some_and(|settings| is_tls_only(&settings)))
}

pub fn remove(ca: &PublicAuthority) -> Result<(), HelperError> {
    let _guard = lock()?;
    let certificate = SecCertificate::from_der(ca.der()).map_err(HelperError::from)?;
    // SAFETY: the certificate owns a valid native reference throughout the call.
    let result = unsafe {
        SecTrustSettingsRemoveTrustSettings(
            certificate.as_concrete_TypeRef(),
            kSecTrustSettingsDomainAdmin,
        )
    };
    if result != errSecItemNotFound && result != errSecNoTrustSettings {
        check(result)?;
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

type Settings = CFDictionary<CFString, CFType>;

fn tls_settings() -> Settings {
    CFDictionary::from_CFType_pairs(&[
        (
            CFString::from_static_string(POLICY),
            SecPolicy::create_ssl(SslProtocolSide::SERVER, None).as_CFType(),
        ),
        (
            CFString::from_static_string(RESULT),
            CFNumber::from(i64::from(kSecTrustSettingsResultTrustRoot)).as_CFType(),
        ),
    ])
}

fn trust_settings(certificate: &SecCertificate) -> Result<Option<CFArray<Settings>>, HelperError> {
    let mut settings = std::ptr::null();
    // SAFETY: the certificate is valid; success returns an owned CFArray of dictionaries.
    let result = unsafe {
        SecTrustSettingsCopyTrustSettings(
            certificate.as_concrete_TypeRef(),
            kSecTrustSettingsDomainAdmin,
            &raw mut settings,
        )
    };
    if result == errSecItemNotFound || result == errSecNoTrustSettings {
        return Ok(None);
    }
    check(result)?;
    // SAFETY: Security.framework returned an owned array after a successful call.
    Ok(Some(unsafe { CFArray::wrap_under_create_rule(settings) }))
}

fn is_tls_only(settings: &CFArray<Settings>) -> bool {
    if settings.len() != 1 {
        return false;
    }
    let entry = settings.get(0).expect("one trust settings dictionary");
    let policy = entry.find(CFString::from_static_string(POLICY));
    let name = entry.find(CFString::from_static_string(POLICY_NAME));
    let result = entry.find(CFString::from_static_string(RESULT));
    entry.len() == 1 + usize::from(name.is_some()) + usize::from(result.is_some())
        && policy.is_some_and(|policy| {
            *policy == SecPolicy::create_ssl(SslProtocolSide::SERVER, None).as_CFType()
        })
        && name.is_none_or(|name| *name == CFString::from_static_string("sslServer").as_CFType())
        && result.is_none_or(|result| {
            result
                .downcast::<CFNumber>()
                .and_then(|number| number.to_i64())
                == Some(i64::from(kSecTrustSettingsResultTrustRoot))
        })
}

fn check(status: i32) -> Result<(), HelperError> {
    if status == 0 {
        Ok(())
    } else {
        Err(security_framework::base::Error::from_code(status).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, GeneralSubtree,
        IsCa, KeyPair, KeyUsagePurpose, NameConstraints,
    };

    #[test]
    fn status_rejects_broad_or_client_trust() {
        let server = tls_settings();
        assert!(is_tls_only(&CFArray::from_CFTypes(std::slice::from_ref(
            &server
        ))));
        assert!(!is_tls_only(&CFArray::from_CFTypes(&[])));
        let broad = Settings::from_CFType_pairs(&[]);
        assert!(!is_tls_only(&CFArray::from_CFTypes(std::slice::from_ref(
            &broad
        ))));
        assert!(!is_tls_only(&CFArray::from_CFTypes(&[server, broad])));
        let client = Settings::from_CFType_pairs(&[(
            CFString::from_static_string(POLICY),
            SecPolicy::create_ssl(SslProtocolSide::CLIENT, None).as_CFType(),
        )]);
        assert!(!is_tls_only(&CFArray::from_CFTypes(&[client])));
    }

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
    #[ignore = "requires root; installs and removes two isolated public CAs in system trust"]
    fn native_trust_lifecycle_preserves_other_roots() {
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
        assert!(!status(&first.0).unwrap());
        install(&first.0).unwrap();
        install(&first.0).unwrap();
        install(&second.0).unwrap();
        assert!(status(&first.0).unwrap());
        assert!(status(&second.0).unwrap());
        remove(&first.0).unwrap();
        remove(&first.0).unwrap();
        assert!(!status(&first.0).unwrap());
        assert!(status(&second.0).unwrap());
    }
}
