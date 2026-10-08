//! Validate the public root before certificate storage or native trust changes.

use arcbox_constants::dns::LOCAL_CA_COMMON_NAME;
use sha2::{Digest, Sha256};
use x509_parser::{
    extensions::{GeneralName, ParsedExtension},
    prelude::{FromDer, X509Certificate},
};

use crate::{CertificateValidity, Error, LOCAL_DOMAIN, MAX_CERTIFICATE_SIZE};

/// A self-signed public CA with the fixed ArcBox identity and signing scope.
/// Expired certificates remain valid inputs so uninstall can revoke their trust.
pub struct PublicAuthority {
    der: Vec<u8>,
    validity: CertificateValidity,
}

impl PublicAuthority {
    /// Parses exactly one PEM certificate and validates the ArcBox root policy.
    pub fn from_pem(bytes: &[u8]) -> Result<Self, Error> {
        let bytes = bytes.trim_ascii();
        if !bytes.starts_with(b"-----BEGIN CERTIFICATE-----")
            || !bytes.ends_with(b"-----END CERTIFICATE-----")
        {
            return Err(Error::InvalidAuthority("expected one PEM certificate"));
        }
        let mut certificates = pem::parse_many(bytes)
            .map_err(|_| Error::InvalidAuthority("invalid PEM certificate"))?;
        if certificates.len() != 1 || certificates[0].tag() != "CERTIFICATE" {
            return Err(Error::InvalidAuthority("expected one PEM certificate"));
        }
        Self::try_from(certificates.remove(0).into_contents())
    }

    /// Returns the validated public certificate. No private key is present.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// Returns the root's validity interval, including expired roots retained for revocation.
    #[must_use]
    pub fn validity(&self) -> CertificateValidity {
        self.validity
    }

    /// Returns the DER certificate's SHA-256 fingerprint as uppercase, colon-separated hex.
    #[must_use]
    pub fn fingerprint_sha256(&self) -> String {
        Sha256::digest(self.der())
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

impl TryFrom<Vec<u8>> for PublicAuthority {
    type Error = Error;

    fn try_from(der: Vec<u8>) -> Result<Self, Self::Error> {
        if der.len() > MAX_CERTIFICATE_SIZE {
            return Err(Error::InvalidAuthority("certificate exceeds 8192 bytes"));
        }
        let (remaining, certificate) = X509Certificate::from_der(&der)
            .map_err(|_| Error::InvalidAuthority("invalid DER certificate"))?;
        if !remaining.is_empty() {
            return Err(Error::InvalidAuthority("trailing certificate data"));
        }
        let subject = certificate.subject();
        if subject.iter_attributes().count() != 2
            || subject
                .iter_common_name()
                .next()
                .and_then(|name| name.as_str().ok())
                != Some(LOCAL_CA_COMMON_NAME)
            || subject
                .iter_organization()
                .next()
                .and_then(|name| name.as_str().ok())
                != Some("ArcBox")
            || certificate.issuer() != subject
        {
            return Err(Error::InvalidAuthority("expected the ArcBox root identity"));
        }
        if certificate.signature_algorithm != certificate.tbs_certificate.signature
            || certificate.signature_value.unused_bits != 0
            || certificate.verify_signature(None).is_err()
        {
            return Err(Error::InvalidAuthority("invalid root self-signature"));
        }
        certificate
            .extensions_map()
            .map_err(|_| Error::InvalidAuthority("duplicate certificate extensions"))?;
        for extension in certificate.extensions() {
            if !matches!(
                extension.parsed_extension(),
                ParsedExtension::BasicConstraints(_)
                    | ParsedExtension::KeyUsage(_)
                    | ParsedExtension::NameConstraints(_)
                    | ParsedExtension::SubjectKeyIdentifier(_)
            ) {
                return Err(Error::InvalidAuthority("unexpected certificate extension"));
            }
        }
        let constraints = certificate
            .basic_constraints()
            .map_err(|_| Error::InvalidAuthority("invalid basic constraints"))?
            .ok_or(Error::InvalidAuthority("missing basic constraints"))?;
        if !constraints.critical
            || !constraints.value.ca
            || constraints.value.path_len_constraint != Some(0)
        {
            return Err(Error::InvalidAuthority("CA must have path length zero"));
        }
        let usage = certificate
            .key_usage()
            .map_err(|_| Error::InvalidAuthority("invalid key usage"))?
            .ok_or(Error::InvalidAuthority("missing key usage"))?;
        if !usage.critical || usage.value.flags != (1 << 5) | (1 << 6) {
            return Err(Error::InvalidAuthority(
                "CA may only sign certificates and CRLs",
            ));
        }
        let names = certificate
            .name_constraints()
            .map_err(|_| Error::InvalidAuthority("invalid name constraints"))?
            .ok_or(Error::InvalidAuthority("missing name constraints"))?;
        let permitted = names
            .value
            .permitted_subtrees
            .as_deref()
            .unwrap_or_default();
        let excluded = names.value.excluded_subtrees.as_deref().unwrap_or_default();
        if !names.critical
            || !matches!(permitted, [name] if name.base == GeneralName::DNSName(LOCAL_DOMAIN))
            || excluded.len() != 2
            || !excluded
                .iter()
                .any(|name| name.base == GeneralName::IPAddress(&[0; 8]))
            || !excluded
                .iter()
                .any(|name| name.base == GeneralName::IPAddress(&[0; 32]))
        {
            return Err(Error::InvalidAuthority(
                "CA must permit only ArcBox DNS names and exclude every IP address",
            ));
        }
        let validity = CertificateValidity::from(certificate.validity());
        Ok(Self { der, validity })
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DnType, GeneralSubtree, IsCa,
        KeyPair, KeyUsagePurpose,
    };

    use super::*;

    fn certificate(params: CertificateParams) -> Vec<u8> {
        params
            .self_signed(&KeyPair::generate().unwrap())
            .unwrap()
            .der()
            .to_vec()
    }

    #[test]
    fn accepts_the_production_root_policy_and_expired_roots_for_revocation() {
        let mut parameters = crate::authority::parameters();
        let der = certificate(parameters.clone());
        let public = PublicAuthority::try_from(der.clone()).unwrap();
        assert_eq!(public.der(), der);
        let pem = pem::encode(&pem::Pem::new("CERTIFICATE", der.clone()));
        assert_eq!(
            PublicAuthority::from_pem(pem.as_bytes()).unwrap().der(),
            der
        );
        assert!(PublicAuthority::from_pem(format!("{pem}{pem}").as_bytes()).is_err());
        parameters.not_after = parameters.not_before;
        parameters.not_before -= time::Duration::days(1);
        assert!(PublicAuthority::try_from(certificate(parameters)).is_ok());
        let mut tampered = der.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(PublicAuthority::try_from(tampered).is_err());
        assert!(PublicAuthority::try_from([der.as_slice(), &[0]].concat()).is_err());
        assert!(PublicAuthority::try_from(vec![0; MAX_CERTIFICATE_SIZE + 1]).is_err());
    }

    #[test]
    fn metadata_identifies_the_certificate_and_keeps_expired_roots_inspectable() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut params = crate::authority::parameters();
        params.not_before = now - time::Duration::days(2);
        params.not_after = now - time::Duration::days(1);
        let key = KeyPair::generate().unwrap();
        let first = params.self_signed(&key).unwrap();
        let public = PublicAuthority::try_from(first.der().to_vec()).unwrap();
        assert_eq!(public.validity().not_before, params.not_before);
        assert_eq!(public.validity().not_after, params.not_after);
        assert!(!public.validity().is_valid_at(now));
        let pem = format!("\n{}\n", first.pem().replace('\n', "\r\n"));
        assert_eq!(
            public.fingerprint_sha256(),
            PublicAuthority::from_pem(pem.as_bytes())
                .unwrap()
                .fingerprint_sha256()
        );
        params.not_after += time::Duration::days(2);
        let replacement =
            PublicAuthority::try_from(params.self_signed(&key).unwrap().der().to_vec()).unwrap();
        assert!(replacement.validity().is_valid_at(now));
        assert_ne!(
            public.fingerprint_sha256(),
            replacement.fingerprint_sha256()
        );
    }

    #[test]
    fn rejects_roots_with_broader_permissions_or_foreign_identity() {
        let edits: &[fn(&mut CertificateParams)] = &[
            |p| p.distinguished_name.push(DnType::CommonName, "Foreign CA"),
            |p| {
                p.distinguished_name
                    .push(DnType::OrganizationName, "Foreign");
            },
            |p| p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained),
            |p| p.is_ca = IsCa::Ca(BasicConstraints::Constrained(1)),
            |p| p.key_usages.push(KeyUsagePurpose::DigitalSignature),
            |p| p.name_constraints = None,
            |p| {
                p.name_constraints
                    .as_mut()
                    .unwrap()
                    .permitted_subtrees
                    .clear();
            },
            |p| {
                p.name_constraints
                    .as_mut()
                    .unwrap()
                    .permitted_subtrees
                    .push(GeneralSubtree::DnsName("example.com".into()));
            },
            |p| {
                p.name_constraints
                    .as_mut()
                    .unwrap()
                    .excluded_subtrees
                    .truncate(1);
            },
        ];
        for (index, edit) in edits.iter().enumerate() {
            let mut parameters = crate::authority::parameters();
            edit(&mut parameters);
            assert!(
                PublicAuthority::try_from(certificate(parameters)).is_err(),
                "policy mutation {index}"
            );
        }

        let good = certificate(crate::authority::parameters());
        let (_, parsed) = X509Certificate::from_der(&good).unwrap();
        let extension = parsed
            .extensions()
            .iter()
            .find(|ext| matches!(ext.parsed_extension(), ParsedExtension::NameConstraints(_)))
            .unwrap();
        let mut parameters = crate::authority::parameters();
        parameters.name_constraints = None;
        // Keep the signed name constraints intact but remove their critical flag.
        parameters
            .custom_extensions
            .push(CustomExtension::from_oid_content(
                &[2, 5, 29, 30],
                extension.value.to_vec(),
            ));
        assert!(PublicAuthority::try_from(certificate(parameters)).is_err());

        let mut parameters = crate::authority::parameters();
        let mut duplicate =
            CustomExtension::from_oid_content(&[2, 5, 29, 30], extension.value.to_vec());
        duplicate.set_criticality(true);
        parameters.custom_extensions.push(duplicate);
        assert!(PublicAuthority::try_from(certificate(parameters)).is_err());
    }
}
