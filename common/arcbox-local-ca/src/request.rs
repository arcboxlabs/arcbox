//! A guest keeps its leaf key and sends only a signed certificate request.

use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair};
#[cfg(any(target_os = "macos", test))]
use rcgen::{
    CertificateSigningRequestParams, ExtendedKeyUsagePurpose, Issuer, KeyUsagePurpose, SanType,
    SigningKey,
};
use rustls::client::{WebPkiServerVerifier, danger::ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
#[cfg(any(target_os = "macos", test))]
use time::{Duration, OffsetDateTime};

#[cfg(any(target_os = "macos", test))]
use crate::CertificateValidity;
use crate::{Error, LOCAL_DOMAIN, PublicAuthority};

/// Maximum DER request or response size accepted across the signing boundary.
pub const MAX_CERTIFICATE_SIZE: usize = 8192;

/// A leaf key that stays in the guest until its certificate arrives.
pub struct CertificateRequest {
    key: KeyPair,
    der: Vec<u8>,
    name: ServerName<'static>,
}

impl CertificateRequest {
    /// Creates a request for one ArcBox DNS name.
    pub fn new(name: &str) -> Result<Self, Error> {
        let server_name = validate_name(name)?.to_owned();
        let key = KeyPair::generate()?;
        let der = CertificateParams::new(vec![name.to_ascii_lowercase()])?
            .serialize_request(&key)?
            .der()
            .to_vec();
        Ok(Self {
            key,
            der,
            name: server_name,
        })
    }

    /// Returns the signed request, which contains no private key.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// Verifies the leaf's name and current root before pairing it with the guest key.
    pub fn complete(
        self,
        certificate: Vec<u8>,
        authority: &PublicAuthority,
    ) -> Result<rustls::ServerConfig, Error> {
        if !authority
            .validity()
            .is_valid_at(std::time::SystemTime::now().into())
        {
            return Err(Error::InvalidAuthority(
                "root certificate is outside its validity period",
            ));
        }
        let certificate = CertificateDer::from(certificate);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(authority.der()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()?
            .verify_server_cert(&certificate, &[], &self.name, &[], UnixTime::now())?;
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der())),
            )?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Validates a DNS name at the signing boundary, including its label syntax.
fn validate_name(name: &str) -> Result<ServerName<'_>, Error> {
    let normalized = name.to_ascii_lowercase();
    let host = normalized
        .strip_suffix(LOCAL_DOMAIN)
        .and_then(|prefix| prefix.strip_suffix('.'));
    if host.is_none_or(str::is_empty) {
        return Err(Error::ForeignName(name.to_owned()));
    }
    match ServerName::try_from(name) {
        Ok(name @ ServerName::DnsName(_)) => Ok(name),
        _ => Err(Error::ForeignName(name.to_owned())),
    }
}

/// A signature-verified CSR with exactly one permitted DNS name.
#[cfg(any(target_os = "macos", test))]
pub struct VerifiedCertificateRequest {
    request: CertificateSigningRequestParams,
    name: String,
}

#[cfg(any(target_os = "macos", test))]
impl TryFrom<&[u8]> for VerifiedCertificateRequest {
    type Error = Error;

    fn try_from(der: &[u8]) -> Result<Self, Error> {
        if der.len() > MAX_CERTIFICATE_SIZE {
            return Err(Error::InvalidRequest(
                "certificate request exceeds 8192 bytes",
            ));
        }
        let request = CertificateSigningRequestParams::from_der(&der.into())?;
        let [SanType::DnsName(name)] = request.params.subject_alt_names.as_slice() else {
            return Err(Error::InvalidRequest("exactly one DNS name is required"));
        };
        validate_name(name.as_str())?;
        let name = name.as_str().to_ascii_lowercase();
        Ok(Self { request, name })
    }
}

#[cfg(any(target_os = "macos", test))]
impl VerifiedCertificateRequest {
    /// Returns the verified, lowercase DNS name for host ownership authorization.
    #[must_use]
    pub fn dns_name(&self) -> &str {
        &self.name
    }
}

/// Applies host-owned certificate policy to an already verified CSR.
#[cfg(any(target_os = "macos", test))]
pub fn sign_request(
    request: &VerifiedCertificateRequest,
    issuer: &Issuer<'_, impl SigningKey>,
    validity: CertificateValidity,
    now: OffsetDateTime,
) -> Result<Vec<u8>, Error> {
    if !validity.is_valid_at(now) {
        return Err(Error::InvalidAuthority(
            "root certificate is outside its validity period",
        ));
    }
    // Copy only the verified public key and DNS name. A guest cannot choose
    // CA privileges, certificate lifetime, or additional certificate usages.
    let mut params = CertificateParams::new(vec![request.name.clone()])?;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    params.not_before = (now - Duration::hours(1)).max(validity.not_before);
    params.not_after = (now + Duration::days(30)).min(validity.not_after);
    Ok(params
        .signed_by(&request.request.public_key, issuer)?
        .der()
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, IsCa};

    fn verified(der: &[u8]) -> VerifiedCertificateRequest {
        VerifiedCertificateRequest::try_from(der).unwrap()
    }

    #[test]
    fn csr_policy_keeps_the_guest_key_and_rejects_foreign_names() {
        let key = KeyPair::generate().unwrap();
        let params = crate::authority::parameters();
        let ca = params.self_signed(&key).unwrap();
        let validity = CertificateValidity::from_der(ca.der()).unwrap();
        let now = OffsetDateTime::now_utc();
        let issuer = Issuer::new(params, key);
        let pending = CertificateRequest::new("dash.signoz.arcbox.local").unwrap();
        let mut tampered = pending.der().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(VerifiedCertificateRequest::try_from(tampered.as_slice()).is_err());
        let request = verified(pending.der());
        assert_eq!(request.dns_name(), "dash.signoz.arcbox.local");
        let certificate = sign_request(&request, &issuer, validity, now).unwrap();
        pending
            .complete(
                certificate.clone(),
                &PublicAuthority::try_from(ca.der().to_vec()).unwrap(),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            rustls::crypto::ring::default_provider().into(),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                &certificate.into(),
                &[],
                &ServerName::try_from("dash.signoz.arcbox.local").unwrap(),
                &[],
                UnixTime::now(),
            )
            .unwrap();

        for name in [
            "example.com",
            "arcbox.local",
            "evilarcbox.local",
            "*.arcbox.local",
            "web.arcbox.local.example.com",
            "127.0.0.1",
        ] {
            assert!(CertificateRequest::new(name).is_err(), "{name}");
            let key = KeyPair::generate().unwrap();
            let request = CertificateParams::new(vec![name.into()])
                .unwrap()
                .serialize_request(&key)
                .unwrap();
            assert!(
                VerifiedCertificateRequest::try_from(request.der().as_ref()).is_err(),
                "{name}"
            );
        }
        let key = KeyPair::generate().unwrap();
        let request = CertificateParams::new(vec!["a.arcbox.local".into(), "example.com".into()])
            .unwrap()
            .serialize_request(&key)
            .unwrap();
        assert!(VerifiedCertificateRequest::try_from(request.der().as_ref()).is_err());
        assert!(VerifiedCertificateRequest::try_from(&[0; MAX_CERTIFICATE_SIZE + 1][..]).is_err());
    }

    #[test]
    fn completion_rejects_a_different_root_or_name_even_when_the_leaf_key_matches() {
        let root = rcgen::CertifiedIssuer::self_signed(
            crate::authority::parameters(),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let other = rcgen::CertifiedIssuer::self_signed(
            crate::authority::parameters(),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let public = PublicAuthority::try_from(root.der().to_vec()).unwrap();
        for (issuer, name) in [(&other, "web.arcbox.local"), (&root, "other.arcbox.local")] {
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            let certificate = CertificateParams::new(vec![name.into()])
                .unwrap()
                .signed_by(&request.key, issuer)
                .unwrap();
            assert!(matches!(
                request.complete(certificate.der().to_vec(), &public),
                Err(Error::Tls(rustls::Error::InvalidCertificate(_)))
            ));
        }
    }

    #[test]
    fn completion_rejects_expired_roots_and_leaves() {
        let now = OffsetDateTime::now_utc();
        for expired_root in [true, false] {
            let mut root_params = crate::authority::parameters();
            if expired_root {
                root_params.not_after = now - Duration::hours(1);
            }
            let root =
                rcgen::CertifiedIssuer::self_signed(root_params, KeyPair::generate().unwrap())
                    .unwrap();
            let public = PublicAuthority::try_from(root.der().to_vec()).unwrap();
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            let mut params = CertificateParams::new(vec!["web.arcbox.local".into()]).unwrap();
            if !expired_root {
                params.not_after = now - Duration::hours(1);
            }
            let certificate = params.signed_by(&request.key, &root).unwrap();
            let result = request.complete(certificate.der().to_vec(), &public);
            if expired_root {
                assert!(matches!(result, Err(Error::InvalidAuthority(_))));
            } else {
                assert!(matches!(
                    result,
                    Err(Error::Tls(rustls::Error::InvalidCertificate(_)))
                ));
            }
        }
    }

    #[test]
    fn guest_cannot_request_ca_or_client_auth_privileges() {
        use rustls::server::WebPkiClientVerifier;
        let ca = rcgen::CertifiedIssuer::self_signed(
            crate::authority::parameters(),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["WEB.arcbox.local".into()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let request = params.serialize_request(&key).unwrap();
        let request = verified(request.der());
        assert_eq!(request.dns_name(), "web.arcbox.local");
        let validity = CertificateValidity::from_der(ca.der()).unwrap();
        let certificate = CertificateDer::from(
            sign_request(&request, &ca, validity, OffsetDateTime::now_utc()).unwrap(),
        );
        let leaf = Issuer::from_ca_cert_der(&certificate, key).unwrap();
        assert_eq!(leaf.key_usages(), [KeyUsagePurpose::DigitalSignature]);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let roots = Arc::new(roots);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        WebPkiServerVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(&provider))
            .build()
            .unwrap()
            .verify_server_cert(
                &certificate,
                &[],
                &ServerName::try_from("web.arcbox.local").unwrap(),
                &[],
                UnixTime::now(),
            )
            .unwrap();
        let error = WebPkiClientVerifier::builder_with_provider(roots, provider)
            .build()
            .unwrap()
            .verify_client_cert(&certificate, &[], UnixTime::now())
            .unwrap_err();
        assert!(format!("{error:?}").contains("InvalidPurpose"), "{error:?}");
    }

    #[test]
    fn root_constraints_reject_foreign_dns_and_ip_even_if_signed() {
        let params = crate::authority::parameters();
        let key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap();
        let issuer = Issuer::new(params, key);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            rustls::crypto::ring::default_provider().into(),
        )
        .build()
        .unwrap();
        for name in ["example.com", "127.0.0.1", "::1"] {
            let leaf = CertificateParams::new(vec![name.into()])
                .unwrap()
                .signed_by(&KeyPair::generate().unwrap(), &issuer)
                .unwrap();
            let error = verifier
                .verify_server_cert(
                    leaf.der(),
                    &[],
                    &ServerName::try_from(name).unwrap(),
                    &[],
                    UnixTime::now(),
                )
                .unwrap_err();
            assert!(
                format!("{error:?}").contains("NameConstraintViolation"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn leaf_validity_cannot_outlive_the_root_or_use_an_inactive_root() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut params = crate::authority::parameters();
        params.not_before = now - Duration::minutes(10);
        params.not_after = now + Duration::days(2);
        let ca = rcgen::CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let validity = CertificateValidity::from_der(ca.der()).unwrap();
        let request = CertificateRequest::new("web.arcbox.local").unwrap();
        let request = verified(request.der());
        let leaf = sign_request(&request, &ca, validity, now).unwrap();
        assert_eq!(CertificateValidity::from_der(&leaf).unwrap(), validity);
        for outside in [
            validity.not_before - Duration::seconds(1),
            validity.not_after + Duration::seconds(1),
        ] {
            assert!(matches!(
                sign_request(&request, &ca, validity, outside),
                Err(Error::InvalidAuthority(_))
            ));
        }
        let renewed = sign_request(&request, &ca, validity, now + Duration::days(1)).unwrap();
        let renewed_validity = CertificateValidity::from_der(&renewed).unwrap();
        assert_eq!(renewed_validity.not_after, validity.not_after);
        assert_eq!(
            renewed_validity.not_before,
            now + Duration::days(1) - Duration::hours(1)
        );
    }
}
