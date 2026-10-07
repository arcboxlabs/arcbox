//! Publish the Keychain certificate only after retiring the shared CA.

use std::{fs, path::Path};

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use core_foundation::base::TCFType;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use security_framework::certificate::SecCertificate;
use security_framework::trust_settings::{Domain, TrustSettings};
use security_framework_sys::base::SecCertificateRef;

use crate::{Error, KeychainAuthority};

unsafe extern "C" {
    fn SecTrustSettingsRemoveTrustSettings(certificate: SecCertificateRef, domain: u32) -> i32;
}

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
        let certificate = self.certificate_pem();
        if current.as_deref() == Some(certificate.as_bytes()) {
            return Ok(());
        }
        if let Some(old) = current {
            remove_trust(&old)?;
        }
        arcbox_atomic_file::write(&path, certificate.as_bytes())?;
        Ok(())
    }
}

/// Removes only this certificate's user trust, preserving every other profile.
pub fn remove_trust(pem: &[u8]) -> Result<(), Error> {
    let der = CertificateDer::from_pem_slice(pem)
        .map_err(|_| Error::InvalidAuthority("invalid public CA certificate"))?;
    let certificate = SecCertificate::from_der(&der)?;
    let trusted = TrustSettings::new(Domain::User)
        .iter()?
        .any(|known| known.to_der() == der.as_ref());
    if trusted {
        // SAFETY: certificate owns a valid SecCertificateRef throughout the call.
        let status = unsafe {
            SecTrustSettingsRemoveTrustSettings(
                certificate.as_concrete_TypeRef(),
                Domain::User as u32,
            )
        };
        if status != 0 {
            return Err(security_framework::base::Error::from_code(status).into());
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
        crate::ensure(dir.path()).unwrap();
        let old = fs::read(dir.path().join(TLS_CA_CERT)).unwrap();
        // Clean up even when an assertion fails. No trust is added by this test.
        struct Cleanup<'a>(&'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                KeychainAuthority::delete(self.0).expect("remove test CA from Keychain");
            }
        }
        let _cleanup = Cleanup(dir.path());
        let authority = KeychainAuthority::open(dir.path()).unwrap();
        authority.publish(dir.path()).unwrap();
        assert!(!dir.path().join(TLS_CA_KEY).exists());
        let new = fs::read(dir.path().join(TLS_CA_CERT)).unwrap();
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
    }
}
