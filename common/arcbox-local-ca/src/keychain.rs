//! The host signs with a persistent Keychain key; no API exports its private bytes.

use std::path::Path;

use rcgen::{Issuer, PKCS_ECDSA_P256_SHA256, PublicKeyData, SignatureAlgorithm, SigningKey};
use security_framework::certificate::SecCertificate;
use security_framework::item::{
    AddRef, ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, KeyClass, Location,
    Reference, SearchResult,
};
use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey};
use security_framework_sys::base::errSecItemNotFound;

use crate::{Error, authority, request};

/// A per-profile CA whose root private key stays in the macOS Keychain.
pub struct KeychainAuthority {
    issuer: Issuer<'static, KeychainKey>,
    certificate: SecCertificate,
}

impl KeychainAuthority {
    /// Opens or creates the profile's key and certificate in the user's keychain.
    /// The default file-keychain ACL trusts the creating application's code identity.
    pub fn open(tls_dir: &Path) -> Result<Self, Error> {
        let label = Self::label(tls_dir)?;
        let key = match find(
            ItemSearchOptions::new()
                .key_class(KeyClass::private())
                .label(&label)
                .load_refs(true),
        )? {
            Some(Reference::Key(key)) => key,
            Some(_) => return Err(Error::InvalidAuthority("expected a private key")),
            None => {
                let mut options = GenerateKeyOptions::default();
                options
                    .set_key_type(KeyType::ec())
                    .set_size_in_bits(256)
                    .set_label(&label)
                    .set_location(Location::DefaultFileKeychain);
                SecKey::new(&options)?
            }
        };
        let public = key
            .public_key()
            .and_then(|key| key.external_representation())
            .ok_or(Error::InvalidAuthority("missing EC public key"))?
            .to_vec();
        let key = KeychainKey { key, public };
        let certificate = match find(
            ItemSearchOptions::new()
                .class(ItemClass::certificate())
                .label(&label)
                .load_refs(true),
        )? {
            Some(Reference::Certificate(certificate)) => certificate,
            Some(_) => return Err(Error::InvalidAuthority("expected a certificate")),
            None => {
                let cert = authority::parameters().self_signed(&key)?;
                let certificate = SecCertificate::from_der(cert.der())?;
                ItemAddOptions::new(ItemAddValue::Ref(AddRef::Certificate(certificate.clone())))
                    .set_label(&label)
                    .set_location(Location::DefaultFileKeychain)
                    .add()?;
                certificate
            }
        };
        let certificate_public = certificate
            .public_key()?
            .external_representation()
            .ok_or(Error::InvalidAuthority("missing certificate public key"))?;
        if certificate_public.bytes() != key.public {
            return Err(Error::InvalidAuthority(
                "certificate and private key differ",
            ));
        }
        let issuer = Issuer::from_ca_cert_der(&certificate.to_der().into(), key)?;
        Ok(Self {
            issuer,
            certificate,
        })
    }

    /// Signs one verified CSR under the host's fixed leaf-certificate policy.
    pub fn sign(&self, csr: &[u8]) -> Result<Vec<u8>, Error> {
        request::sign_request(csr, &self.issuer)
    }

    /// Exports only the public root certificate for client trust.
    #[must_use]
    pub fn certificate_pem(&self) -> String {
        pem::encode(&pem::Pem::new("CERTIFICATE", self.certificate.to_der()))
    }

    /// Returns the profile-specific keychain label, independent of executable path.
    pub fn label(tls_dir: &Path) -> Result<String, Error> {
        let dir = tls_dir.canonicalize().map_err(|e| Error::io(tls_dir, e))?;
        Ok(format!("dev.arcbox.local-ca.v2:{}", dir.display()))
    }

    /// Deletes this profile's keys and public certificate after trust removal.
    pub fn delete(tls_dir: &Path) -> Result<(), Error> {
        let label = Self::label(tls_dir)?;
        for class in [ItemClass::key(), ItemClass::certificate()] {
            match ItemSearchOptions::new().class(class).label(&label).delete() {
                Ok(()) => {}
                Err(error) if error.code() == errSecItemNotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn find(query: &ItemSearchOptions) -> Result<Option<Reference>, Error> {
    match query.search() {
        Ok(items) => match items.into_iter().next() {
            Some(SearchResult::Ref(reference)) => Ok(Some(reference)),
            _ => Err(Error::InvalidAuthority("expected a keychain reference")),
        },
        Err(error) if error.code() == errSecItemNotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct KeychainKey {
    key: SecKey,
    public: Vec<u8>,
}

impl PublicKeyData for KeychainKey {
    fn der_bytes(&self) -> &[u8] {
        &self.public
    }
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

impl SigningKey for KeychainKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.key
            .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
            .map_err(|error| {
                // rcgen's signing trait cannot carry platform errors. Preserve
                // the platform diagnostic before returning its remote-key error.
                tracing::error!(%error, "Keychain CA signing failed");
                rcgen::Error::RemoteKeyError
            })
    }
}
