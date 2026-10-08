//! Publish the Keychain certificate and explicitly repair its public file without changing trust.

use std::{fs, path::Path};

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use security_framework::certificate::SecCertificate;
use security_framework::item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::trust_settings::Domain;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::{Error, KeychainAuthority, PublicAuthority, trust};

/// Whether the current public root replaced the previous publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publication {
    /// The current public root is available to clients.
    Published,
    /// The previous root remains trusted and must be revoked in the login session.
    AwaitingRevocation,
}

/// Removes an exposed root key before the data directory is shared with a guest.
pub fn remove_shared_private_key(dir: &Path) -> Result<(), Error> {
    let key = dir.join(TLS_CA_KEY);
    match fs::remove_file(&key) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(&key, error)),
    }
}

impl KeychainAuthority {
    /// Publishes public bytes without requesting native authorization.
    pub fn publish(&self, dir: &Path) -> Result<Publication, Error> {
        PublicAuthority::from_pem(self.certificate_pem().as_bytes())?.publish(dir)
    }
}

impl PublicAuthority {
    /// Restores only this profile's current public root without creating a key or changing trust.
    /// Preserves every recoverable different certificate for explicit trust revocation.
    pub fn repair_publication(&self, dir: &Path) -> Result<(), Error> {
        if KeychainAuthority::existing_certificate(dir)?.as_deref() != Some(self.der()) {
            return Err(Error::InvalidAuthority(
                "publication repair requires the current profile's Keychain CA",
            ));
        }
        self.repair_checked(dir)
    }

    fn repair_checked(&self, dir: &Path) -> Result<(), Error> {
        if !self.validity().is_valid_at(time::OffsetDateTime::now_utc()) {
            return Err(Error::InvalidAuthority(
                "the current CA is outside its validity period; rotate it before repairing publication",
            ));
        }
        let path = dir.join(TLS_CA_CERT);
        match fs::read(&path) {
            Ok(bytes) => {
                self.check_repairable(&bytes)?;
                if Self::from_pem(&bytes).is_ok() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
        fs::create_dir_all(dir).map_err(|error| Error::io(dir, error))?;
        let exported = pem::encode(&pem::Pem::new("CERTIFICATE", self.der()));
        arcbox_atomic_file::write(&path, exported.as_bytes())?;
        Ok(())
    }

    fn check_repairable(&self, bytes: &[u8]) -> Result<(), Error> {
        // Inspect raw DER before PEM so an appended PEM cannot hide a complete certificate.
        if X509Certificate::from_der(bytes).is_ok() {
            return self.check_repair_certificate(bytes);
        }
        let certificates = pem::parse_many(bytes).map_err(|_| {
            Error::InvalidAuthority(
                "ambiguous PEM publication; preserve it for certificate recovery",
            )
        })?;
        if certificates.len() > 1 {
            return Err(Error::InvalidAuthority(
                "publication contains multiple PEM objects; repair cannot replace them",
            ));
        }
        if let Some(certificate) = certificates.first() {
            if certificate.tag() != "CERTIFICATE" {
                return Err(Error::InvalidAuthority(
                    "publication contains a non-certificate PEM object",
                ));
            }
            self.check_repair_certificate(certificate.contents())?;
        }
        Ok(())
    }

    fn check_repair_certificate(&self, der: &[u8]) -> Result<(), Error> {
        // Structural parsing protects foreign and policy-invalid certificates without trusting them.
        if X509Certificate::from_der(der).is_ok() && der != self.der() {
            return Err(Error::InvalidAuthority(
                "publication contains a different certificate; revoke its trust before replacing it",
            ));
        }
        Ok(())
    }

    /// Publishes this root after the previous root loses User and Admin trust.
    /// Preserves a trusted previous root so an explicit action can revoke the exact certificate.
    pub fn publish(&self, dir: &Path) -> Result<Publication, Error> {
        self.publish_checked(dir, Self::has_trust)
    }

    fn publish_checked(
        &self,
        dir: &Path,
        has_trust: impl FnOnce(&Self) -> Result<bool, Error>,
    ) -> Result<Publication, Error> {
        remove_shared_private_key(dir)?;
        let path = dir.join(TLS_CA_CERT);
        match fs::read(&path) {
            Ok(bytes) => {
                let previous = Self::from_pem(&bytes)?;
                if previous.der() == self.der() {
                    return Ok(Publication::Published);
                }
                if has_trust(&previous)? {
                    return Ok(Publication::AwaitingRevocation);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
        let exported = pem::encode(&pem::Pem::new("CERTIFICATE", self.der()));
        arcbox_atomic_file::write(&path, exported.as_bytes())?;
        Ok(Publication::Published)
    }
}

/// Removes only this certificate's user trust, preserving every other profile.
/// The certificate does not need to remain stored in the Keychain.
pub fn remove_trust(pem: &[u8]) -> Result<(), Error> {
    trust::remove(&certificate(pem)?, Domain::User)
}

/// Removes only this DER certificate's user trust, preserving every other profile.
/// The certificate does not need to remain stored in the Keychain.
pub fn remove_user_trust_der(der: &[u8]) -> Result<(), Error> {
    trust::remove(&SecCertificate::from_der(der)?, Domain::User)
}

fn certificate(pem: &[u8]) -> Result<SecCertificate, Error> {
    let der = CertificateDer::from_pem_slice(pem)
        .map_err(|_| Error::InvalidAuthority("invalid public CA certificate"))?;
    Ok(SecCertificate::from_der(&der)?)
}

pub fn delete_certificate(pem: &[u8]) -> Result<(), Error> {
    let certificate = certificate(pem)?;
    trust::remove(&certificate, Domain::User)?;
    // A certificate parsed from DER has no persistent reference. Delete the
    // matching Keychain reference, never another certificate with the same serial.
    let found = ItemSearchOptions::new()
        .keychains(&[SecKeychain::default()?])
        .class(ItemClass::certificate())
        .serial_number(&certificate.serial_number_bytes()?)
        .limit(Limit::All)
        .load_refs(true)
        .search();
    let items = match found {
        Ok(items) => items,
        Err(error) if error.code() == security_framework_sys::base::errSecItemNotFound => {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    for item in items {
        let SearchResult::Ref(Reference::Certificate(stored)) = item else {
            return Err(Error::InvalidAuthority("expected a certificate reference"));
        };
        if stored.to_der() == certificate.to_der() {
            stored.delete()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CertificateRequest;

    fn public_root(parameters: rcgen::CertificateParams) -> (PublicAuthority, String) {
        let certificate = parameters
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap();
        (
            PublicAuthority::try_from(certificate.der().to_vec()).unwrap(),
            certificate.pem(),
        )
    }

    #[test]
    fn repair_restores_missing_or_damaged_publication_with_the_same_root() {
        let dir = tempfile::tempdir().unwrap();
        let (current, pem) = public_root(crate::authority::parameters());
        let tls = dir.path().join("missing-tls");
        let path = tls.join(TLS_CA_CERT);
        current.repair_checked(&tls).unwrap();
        for damaged in [
            Vec::new(),
            b"corrupt certificate".to_vec(),
            pem.as_bytes()[..40].to_vec(),
            pem::encode(&pem::Pem::new("CERTIFICATE", [0_u8])).into_bytes(),
            current.der().to_vec(),
            format!("damaged prefix\n{pem}").into_bytes(),
        ] {
            fs::write(&path, damaged).unwrap();
            current.repair_checked(&tls).unwrap();
            assert_eq!(
                PublicAuthority::from_pem(&fs::read(&path).unwrap())
                    .unwrap()
                    .der(),
                current.der()
            );
        }
        let published = fs::read(&path).unwrap();
        current.repair_checked(&tls).unwrap();
        assert_eq!(fs::read(&path).unwrap(), published);
    }

    #[test]
    fn repair_preserves_different_certificates_and_ambiguous_pem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TLS_CA_CERT);
        let (current, current_pem) = public_root(crate::authority::parameters());
        let (different, different_pem) = public_root(crate::authority::parameters());
        let mut broad = crate::authority::parameters();
        broad.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let policy_invalid = broad
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap();
        let foreign = rcgen::CertificateParams::default()
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap();
        for preserved in [
            different.der().to_vec(),
            different_pem.into_bytes(),
            foreign.pem().into_bytes(),
            policy_invalid.pem().into_bytes(),
            format!("{current_pem}{current_pem}").into_bytes(),
            format!("{current_pem}{}", foreign.pem()).into_bytes(),
            format!("-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----\n{current_pem}")
                .into_bytes(),
            [different.der(), current_pem.as_bytes()].concat(),
        ] {
            fs::write(&path, &preserved).unwrap();
            assert!(current.repair_checked(dir.path()).is_err());
            assert_eq!(fs::read(&path).unwrap(), preserved);
        }
    }

    #[test]
    fn repair_rejects_an_expired_root_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TLS_CA_CERT);
        fs::write(&path, b"damaged").unwrap();
        let mut parameters = crate::authority::parameters();
        parameters.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        parameters.not_before = parameters.not_after - time::Duration::days(1);
        let (expired, _) = public_root(parameters);
        assert!(expired.repair_checked(dir.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"damaged");
    }

    #[test]
    fn publication_preserves_a_trusted_previous_root_until_explicit_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let root = || {
            let key = rcgen::KeyPair::generate().unwrap();
            crate::authority::parameters().self_signed(&key).unwrap()
        };
        let previous = root();
        let current = PublicAuthority::try_from(root().der().to_vec()).unwrap();
        let path = dir.path().join(TLS_CA_CERT);
        fs::write(&path, previous.pem()).unwrap();
        fs::write(dir.path().join(TLS_CA_KEY), b"retired private key").unwrap();
        assert_eq!(
            current.publish_checked(dir.path(), |_| Ok(true)).unwrap(),
            Publication::AwaitingRevocation
        );
        assert_eq!(fs::read(&path).unwrap(), previous.pem().as_bytes());
        assert!(!dir.path().join(TLS_CA_KEY).exists());

        assert_eq!(
            current.publish_checked(dir.path(), |_| Ok(false)).unwrap(),
            Publication::Published
        );
        assert_eq!(
            PublicAuthority::from_pem(&fs::read(&path).unwrap())
                .unwrap()
                .der(),
            current.der()
        );
        current
            .publish_checked(dir.path(), |_| {
                panic!("same root needs no trust inspection")
            })
            .unwrap();
    }

    #[test]
    fn failed_trust_inspection_preserves_the_previous_publication() {
        let dir = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let previous = crate::authority::parameters().self_signed(&key).unwrap();
        let other_key = rcgen::KeyPair::generate().unwrap();
        let current = PublicAuthority::try_from(
            crate::authority::parameters()
                .self_signed(&other_key)
                .unwrap()
                .der()
                .to_vec(),
        )
        .unwrap();
        let path = dir.path().join(TLS_CA_CERT);
        fs::write(&path, previous.pem()).unwrap();
        assert!(
            current
                .publish_checked(dir.path(), |_| Err(Error::InvalidAuthority(
                    "inspection failed"
                )))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), previous.pem().as_bytes());
    }

    #[test]
    #[ignore = "creates and deletes an isolated CA in the user's macOS Keychain"]
    fn migration_rotates_the_exposed_key_and_reopens_the_keychain_ca() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_key = rcgen::KeyPair::generate().unwrap();
        let legacy_cert = crate::authority::parameters()
            .self_signed(&legacy_key)
            .unwrap();
        fs::write(dir.path().join(TLS_CA_KEY), legacy_key.serialize_pem()).unwrap();
        fs::write(dir.path().join(TLS_CA_CERT), legacy_cert.pem()).unwrap();
        let old = fs::read(dir.path().join(TLS_CA_CERT)).unwrap();
        // Clean up even when an assertion fails. No trust is added by this test.
        struct Cleanup<'a>(&'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                KeychainAuthority::delete(self.0).expect("remove test CA from Keychain");
            }
        }
        let _cleanup = Cleanup(dir.path());
        assert!(
            KeychainAuthority::existing_certificate(dir.path())
                .unwrap()
                .is_none()
        );
        let authority = KeychainAuthority::open(dir.path()).unwrap();
        authority.publish(dir.path()).unwrap();
        assert!(!dir.path().join(TLS_CA_KEY).exists());
        let new = fs::read(dir.path().join(TLS_CA_CERT)).unwrap();
        assert_eq!(
            KeychainAuthority::existing_certificate(dir.path())
                .unwrap()
                .unwrap(),
            CertificateDer::from_pem_slice(&new).unwrap().as_ref()
        );
        assert_ne!(new, old);
        drop(authority);
        let reopened = KeychainAuthority::open(dir.path()).unwrap();
        reopened.publish(dir.path()).unwrap();
        assert_eq!(fs::read(dir.path().join(TLS_CA_CERT)).unwrap(), new);
        let pending = CertificateRequest::new("test.arcbox.local").unwrap();
        let request = crate::VerifiedCertificateRequest::try_from(pending.der()).unwrap();
        let certificate = reopened.sign(&request).unwrap();
        use rustls::client::danger::ServerCertVerifier as _;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(&new).unwrap())
            .unwrap();
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            std::sync::Arc::new(roots),
            rustls::crypto::ring::default_provider().into(),
        )
        .build()
        .unwrap()
        .verify_server_cert(
            &certificate.clone().into(),
            &[],
            &rustls::pki_types::ServerName::try_from("test.arcbox.local").unwrap(),
            &[],
            rustls::pki_types::UnixTime::now(),
        )
        .unwrap();
        pending
            .complete(certificate, &PublicAuthority::from_pem(&new).unwrap())
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        let _other_cleanup = Cleanup(other.path());
        let other_ca = KeychainAuthority::open(other.path()).unwrap();
        let other_pem = other_ca.certificate_pem();
        assert_ne!(reopened.certificate_pem(), other_pem);
        let public = PublicAuthority::from_pem(&new).unwrap();
        fs::write(dir.path().join(TLS_CA_CERT), b"damaged").unwrap();
        public.repair_publication(dir.path()).unwrap();
        assert_eq!(fs::read(dir.path().join(TLS_CA_CERT)).unwrap(), new);
        assert!(public.repair_publication(other.path()).is_err());
        assert_eq!(other_ca.certificate_pem(), other_pem);
        let path = dir.path().to_owned();
        fs::remove_dir_all(&path).unwrap();
        assert!(
            KeychainAuthority::existing_certificate(&path)
                .unwrap()
                .is_some()
        );
        KeychainAuthority::delete(&path).unwrap();
        assert!(
            KeychainAuthority::existing_certificate(&path)
                .unwrap()
                .is_none()
        );
        let remaining = ItemSearchOptions::new()
            .class(ItemClass::key())
            .label(&KeychainAuthority::label(&path).unwrap())
            .load_refs(true)
            .search();
        assert_eq!(
            remaining.unwrap_err().code(),
            security_framework_sys::base::errSecItemNotFound
        );
        fs::create_dir(&path).unwrap();
        let replacement = KeychainAuthority::open(&path).unwrap();
        assert_ne!(replacement.certificate_pem().as_bytes(), new);
        assert_eq!(
            KeychainAuthority::open(other.path())
                .unwrap()
                .certificate_pem(),
            other_pem
        );
    }
}
