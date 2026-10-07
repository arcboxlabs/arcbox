//! The host signs with a persistent Keychain key; no API exports its private bytes.

use std::path::Path;

use core_foundation::{base::TCFType, data::CFData};
use rcgen::{Issuer, PKCS_ECDSA_P256_SHA256, PublicKeyData, SignatureAlgorithm, SigningKey};
use security_framework::certificate::SecCertificate;
use security_framework::item::{
    AddRef, ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, KeyClass, Limit, Location,
    Reference, SearchResult,
};
use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey};
use security_framework::os::macos::keychain::SecKeychain;
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
        let existing = certificate_for_key(&key)?;
        let key = KeychainKey { key, public };
        let certificate = match existing {
            Some(certificate) => certificate,
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

    /// Finds this profile's existing public certificate without creating or signing anything.
    pub fn existing_certificate(tls_dir: &Path) -> Result<Option<Vec<u8>>, Error> {
        match find(
            ItemSearchOptions::new()
                .key_class(KeyClass::private())
                .label(&Self::label(tls_dir)?)
                .load_refs(true),
        )? {
            Some(Reference::Key(key)) => Ok(certificate_for_key(&key)?.map(|cert| cert.to_der())),
            Some(_) => Err(Error::InvalidAuthority("expected a private key")),
            None => Ok(None),
        }
    }

    // Resolve surviving ancestors so uninstall can find keys after the TLS directory is removed.
    pub(crate) fn label(tls_dir: &Path) -> Result<String, Error> {
        let path = std::path::absolute(tls_dir).map_err(|e| Error::io(tls_dir, e))?;
        for ancestor in path.ancestors() {
            match ancestor.canonicalize() {
                Ok(mut root) => {
                    let suffix = path
                        .strip_prefix(ancestor)
                        .expect("ancestor is a path prefix");
                    if !suffix.as_os_str().is_empty() {
                        root.push(suffix);
                    }
                    return Ok(format!("dev.arcbox.local-ca.v2:{}", root.display()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(ancestor, e)),
            }
        }
        Err(Error::InvalidAuthority("CA path has no existing ancestor"))
    }

    /// Revokes this profile's trust and deletes its keys and public certificate.
    pub fn delete(tls_dir: &Path) -> Result<(), Error> {
        let label = Self::label(tls_dir)?;
        let path = tls_dir.join(arcbox_constants::paths::guest::TLS_CA_CERT);
        match std::fs::read(&path) {
            Ok(pem) => crate::migration::delete_certificate(&pem)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
        if let Some(Reference::Key(key)) = find(
            ItemSearchOptions::new()
                .key_class(KeyClass::private())
                .label(&label)
                .load_refs(true),
        )? {
            if let Some(certificate) = certificate_for_key(&key)? {
                crate::remove_user_trust_der(&certificate.to_der())?;
                certificate.delete()?;
            }
        }
        match ItemSearchOptions::new()
            .keychains(&[SecKeychain::default()?])
            .class(ItemClass::key())
            .label(&label)
            .limit(Limit::All)
            .delete()
        {
            Ok(()) => {}
            Err(error) if error.code() == errSecItemNotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

fn certificate_for_key(key: &SecKey) -> Result<Option<SecCertificate>, Error> {
    let hash = public_key_hash(key)?;
    match find(
        ItemSearchOptions::new()
            .class(ItemClass::certificate())
            .pub_key_hash(hash.bytes())
            .load_refs(true),
    )? {
        Some(Reference::Certificate(certificate)) => Ok(Some(certificate)),
        Some(_) => Err(Error::InvalidAuthority("expected a certificate")),
        None => Ok(None),
    }
}

fn public_key_hash(key: &SecKey) -> Result<CFData, Error> {
    let attributes = key.attributes();
    // SAFETY: Security.framework defines this constant as a CFString dictionary key.
    let label = unsafe { security_framework_sys::item::kSecAttrApplicationLabel };
    let value = attributes
        .find(label.cast())
        .ok_or(Error::InvalidAuthority("missing public key hash"))?;
    // SAFETY: SecKeyCopyAttributes returns kSecAttrApplicationLabel as CFData.
    Ok(unsafe { CFData::wrap_under_get_rule((*value).cast()) })
}

fn find(query: &mut ItemSearchOptions) -> Result<Option<Reference>, Error> {
    // The helper stores a public copy in System.keychain. Identity operations
    // must only find and delete the user's original Keychain items.
    match query.keychains(&[SecKeychain::default()?]).search() {
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
