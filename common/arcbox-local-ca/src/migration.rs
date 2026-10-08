//! Publish the Keychain certificate only after retiring the shared CA.

use std::{fs, path::Path};

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use security_framework::certificate::SecCertificate;
use security_framework::item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::trust_settings::Domain;

use crate::{Error, KeychainAuthority, trust};

impl KeychainAuthority {
    /// Retires any shared CA key before the VM boots and publishes this root.
    /// A failed trust removal leaves the old public certificate for the next attempt.
    pub fn publish(&self, dir: &Path) -> Result<(), Error> {
        let key = dir.join(TLS_CA_KEY);
        match fs::remove_file(&key) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&key, error)),
        }
        let path = dir.join(TLS_CA_CERT);
        let current = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::io(&path, error)),
        };
        let exported = self.certificate_pem();
        if current.as_deref() == Some(exported.as_bytes()) {
            return Ok(());
        }
        if let Some(old) = current {
            if certificate(&old)?.to_der() != certificate(exported.as_bytes())?.to_der() {
                remove_trust(&old)?;
            }
        }
        arcbox_atomic_file::write(&path, exported.as_bytes())?;
        Ok(())
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
        pending.complete(certificate).unwrap();
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
