//! Publish the Keychain certificate only after retiring the shared CA.

use std::{fs, path::Path};

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use security_framework::certificate::SecCertificate;
use security_framework::item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::trust_settings::Domain;

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
        let certificate = reopened.sign(pending.der()).unwrap();
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
