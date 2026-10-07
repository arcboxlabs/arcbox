//! A guest keeps its leaf key and sends only a signed certificate request.

use rcgen::{CertificateParams, KeyPair};
#[cfg(any(target_os = "macos", test))]
use rcgen::{
    CertificateSigningRequestParams, ExtendedKeyUsagePurpose, Issuer, KeyUsagePurpose, SanType,
    SigningKey,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
#[cfg(any(target_os = "macos", test))]
use time::{Duration, OffsetDateTime};

use crate::{Error, LOCAL_DOMAIN};

/// Maximum DER request or response size accepted across the signing boundary.
pub const MAX_CERTIFICATE_SIZE: usize = 8192;

/// A leaf key that stays in the guest until its certificate arrives.
pub struct CertificateRequest {
    key: KeyPair,
    der: Vec<u8>,
}

impl CertificateRequest {
    /// Creates a request for one ArcBox DNS name.
    pub fn new(name: &str) -> Result<Self, Error> {
        validate_name(name)?;
        let key = KeyPair::generate()?;
        let der = CertificateParams::new(vec![name.to_ascii_lowercase()])?
            .serialize_request(&key)?
            .der()
            .to_vec();
        Ok(Self { key, der })
    }

    /// Returns the signed request, which contains no private key.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// Pairs the host's certificate with the guest key and checks that they match.
    pub fn complete(self, certificate: Vec<u8>) -> Result<rustls::ServerConfig, Error> {
        let provider = rustls::crypto::ring::default_provider();
        let mut config = rustls::ServerConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der())),
            )?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Validates a DNS name at the signing boundary, including its label syntax.
fn validate_name(name: &str) -> Result<(), Error> {
    let normalized = name.to_ascii_lowercase();
    let host = normalized
        .strip_suffix(LOCAL_DOMAIN)
        .and_then(|prefix| prefix.strip_suffix('.'));
    if host.is_none_or(str::is_empty)
        || !matches!(ServerName::try_from(name), Ok(ServerName::DnsName(_)))
    {
        return Err(Error::ForeignName(name.to_owned()));
    }
    Ok(())
}

/// Verifies the CSR signature and applies host-owned certificate policy.
#[cfg(any(target_os = "macos", test))]
pub fn sign_request(der: &[u8], issuer: &Issuer<'_, impl SigningKey>) -> Result<Vec<u8>, Error> {
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
    // Copy only the verified public key and DNS name. A guest cannot choose
    // CA privileges, certificate lifetime, or additional certificate usages.
    let mut params = CertificateParams::new(vec![name.as_str().to_ascii_lowercase()])?;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(30);
    Ok(params
        .signed_by(&request.public_key, issuer)?
        .der()
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, IsCa};
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::UnixTime;
    use std::sync::Arc;

    #[test]
    fn csr_policy_keeps_the_guest_key_and_rejects_foreign_names() {
        let key = KeyPair::generate().unwrap();
        let params = crate::authority::parameters();
        let ca = params.self_signed(&key).unwrap();
        let issuer = Issuer::new(params, key);
        let pending = CertificateRequest::new("dash.signoz.arcbox.local").unwrap();
        let mut tampered = pending.der().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(sign_request(&tampered, &issuer).is_err());
        let certificate = sign_request(pending.der(), &issuer).unwrap();
        pending.complete(certificate.clone()).unwrap();
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
            assert!(sign_request(request.der(), &issuer).is_err(), "{name}");
        }
        let key = KeyPair::generate().unwrap();
        let request = CertificateParams::new(vec!["a.arcbox.local".into(), "example.com".into()])
            .unwrap()
            .serialize_request(&key)
            .unwrap();
        assert!(sign_request(request.der(), &issuer).is_err());
        assert!(sign_request(&[0; MAX_CERTIFICATE_SIZE + 1], &issuer).is_err());
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
        let mut params = CertificateParams::new(vec!["web.arcbox.local".into()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let request = params.serialize_request(&key).unwrap();
        let certificate = CertificateDer::from(sign_request(request.der(), &ca).unwrap());
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
}
