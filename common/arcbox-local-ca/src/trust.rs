//! Native trust settings. Admin mutations require a process in the login session.

use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use security_framework::{
    certificate::SecCertificate, policy::SecPolicy, secure_transport::SslProtocolSide,
    trust_settings::Domain,
};
use security_framework_sys::{
    base::{SecCertificateRef, errSecItemNotFound, errSecNoTrustSettings},
    trust_settings::{
        SecTrustSettingsCopyTrustSettings, SecTrustSettingsSetTrustSettings,
        kSecTrustSettingsResultTrustRoot,
    },
};

use crate::{Error, PublicAuthority};

const POLICY: &str = "kSecTrustSettingsPolicy";
const POLICY_NAME: &str = "kSecTrustSettingsPolicyName";
const RESULT: &str = "kSecTrustSettingsResult";
type Settings = CFDictionary<CFString, CFType>;

unsafe extern "C" {
    fn SecTrustSettingsRemoveTrustSettings(certificate: SecCertificateRef, domain: u32) -> i32;
}

impl PublicAuthority {
    /// Grants TLS-server-only Admin trust through the native authorization dialog.
    /// Existing matching settings require no authorization.
    pub fn trust_admin(&self) -> Result<(), Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        if trust_settings(&certificate, Domain::Admin)?
            .is_some_and(|settings| is_tls_only(&settings))
        {
            return Ok(());
        }
        let settings = tls_settings();
        // SAFETY: the owned certificate and dictionary remain valid for the native call.
        check(unsafe {
            SecTrustSettingsSetTrustSettings(
                certificate.as_concrete_TypeRef(),
                Domain::Admin as u32,
                settings.as_CFTypeRef(),
            )
        })
    }

    /// Returns whether Admin trust is restricted to the TLS server policy.
    pub fn admin_trusted(&self) -> Result<bool, Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        Ok(trust_settings(&certificate, Domain::Admin)?
            .is_some_and(|settings| is_tls_only(&settings)))
    }

    /// Returns whether any Admin trust settings exist, including deny or broad trust.
    pub fn has_admin_trust(&self) -> Result<bool, Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        Ok(trust_settings(&certificate, Domain::Admin)?.is_some())
    }

    /// Returns whether any User trust settings exist for this certificate.
    pub fn has_user_trust(&self) -> Result<bool, Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        Ok(trust_settings(&certificate, Domain::User)?.is_some())
    }

    /// Returns whether any User or Admin trust settings exist for this certificate.
    pub fn has_trust(&self) -> Result<bool, Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        Ok(trust_settings(&certificate, Domain::User)?.is_some()
            || trust_settings(&certificate, Domain::Admin)?.is_some())
    }

    /// Revokes only this root's Admin trust through native authorization.
    /// An absent entry requires no authorization.
    pub fn remove_admin_trust(&self) -> Result<(), Error> {
        let certificate = SecCertificate::from_der(self.der())?;
        if trust_settings(&certificate, Domain::Admin)?.is_none() {
            return Ok(());
        }
        // SAFETY: the certificate owns a valid native reference throughout the call.
        let result = unsafe {
            SecTrustSettingsRemoveTrustSettings(
                certificate.as_concrete_TypeRef(),
                Domain::Admin as u32,
            )
        };
        if result == errSecItemNotFound || result == errSecNoTrustSettings {
            return Ok(());
        }
        check(result)
    }
}

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

fn trust_settings(
    certificate: &SecCertificate,
    domain: Domain,
) -> Result<Option<CFArray<Settings>>, Error> {
    let mut settings = std::ptr::null();
    // SAFETY: the certificate is valid; success returns an owned CFArray of dictionaries.
    let result = unsafe {
        SecTrustSettingsCopyTrustSettings(
            certificate.as_concrete_TypeRef(),
            domain as u32,
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

fn check(status: i32) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(security_framework::base::Error::from_code(status).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn status_accepts_native_normalization_but_rejects_policy_overrides() {
        let policy = (
            CFString::from_static_string(POLICY),
            SecPolicy::create_ssl(SslProtocolSide::SERVER, None).as_CFType(),
        );
        let name = (
            CFString::from_static_string(POLICY_NAME),
            CFString::from_static_string("sslServer").as_CFType(),
        );
        let normalized = Settings::from_CFType_pairs(&[policy.clone(), name]);
        assert!(is_tls_only(&CFArray::from_CFTypes(&[normalized])));
        for (key, value) in [
            (
                POLICY_NAME,
                CFString::from_static_string("sslClient").as_CFType(),
            ),
            (RESULT, CFNumber::from(3_i64).as_CFType()),
            (
                "kSecTrustSettingsAllowedError",
                CFNumber::from(-1_i64).as_CFType(),
            ),
        ] {
            let settings = Settings::from_CFType_pairs(&[
                policy.clone(),
                (CFString::from_static_string(key), value),
            ]);
            assert!(!is_tls_only(&CFArray::from_CFTypes(&[settings])), "{key}");
        }
    }

    #[test]
    #[ignore = "requires a macOS login session; prompts to trust and revoke two isolated CAs"]
    fn native_admin_trust_lifecycle_preserves_other_roots() {
        struct Cleanup(PublicAuthority);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0
                    .remove_admin_trust()
                    .expect("revoke isolated test CA");
            }
        }
        let authority = || {
            Cleanup(
                PublicAuthority::try_from(
                    crate::authority::parameters()
                        .self_signed(&rcgen::KeyPair::generate().unwrap())
                        .unwrap()
                        .der()
                        .to_vec(),
                )
                .unwrap(),
            )
        };
        let first = authority();
        let second = authority();
        assert!(!first.0.has_trust().unwrap());
        assert!(!first.0.admin_trusted().unwrap());
        first.0.trust_admin().unwrap();
        first.0.trust_admin().unwrap();
        second.0.trust_admin().unwrap();
        assert!(first.0.has_trust().unwrap());
        assert!(first.0.has_admin_trust().unwrap());
        assert!(first.0.admin_trusted().unwrap());
        assert!(second.0.admin_trusted().unwrap());
        first.0.remove_admin_trust().unwrap();
        first.0.remove_admin_trust().unwrap();
        assert!(!first.0.has_trust().unwrap());
        assert!(!first.0.admin_trusted().unwrap());
        assert!(second.0.admin_trusted().unwrap());
        second.0.remove_admin_trust().unwrap();
        assert!(!second.0.has_trust().unwrap());
    }
}
