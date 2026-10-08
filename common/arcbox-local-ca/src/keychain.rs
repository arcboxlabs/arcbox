//! The journal selects a persistent Keychain root; no API exports its private bytes.

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

use crate::journal::{Journal, Root, Slot};
use crate::{Error, PublicAuthority, VerifiedCertificateRequest, authority, request};

mod cleanup;

/// A per-profile CA whose root private key stays in the macOS Keychain.
pub struct KeychainAuthority {
    issuer: Issuer<'static, KeychainKey>,
    certificate: SecCertificate,
    public: PublicAuthority,
}

impl KeychainAuthority {
    /// Opens or creates the profile's key and certificate in the user's keychain.
    /// The default file-keychain ACL trusts the creating application's code identity.
    pub fn open(tls_dir: &Path) -> Result<Self, Error> {
        if let Some(journal) = Self::journal(tls_dir)? {
            return Self::load_root(tls_dir, journal.current());
        }
        let authority = Self::open_slot(tls_dir, Slot::Primary, true)?;
        Journal::Stable {
            current: authority.root(Slot::Primary),
        }
        .persist(tls_dir)?;
        Ok(authority)
    }

    pub(crate) fn open_slot(tls_dir: &Path, slot: Slot, create: bool) -> Result<Self, Error> {
        let label = Self::slot_label(tls_dir, slot)?;
        let key = match find_key(&label)? {
            Some(key) => key,
            None if !create => {
                return Err(Error::InvalidAuthority(
                    "recorded CA private key is missing",
                ));
            }
            None => {
                if Self::slot_has_keys(tls_dir, slot)? {
                    return Err(Error::InvalidAuthority(
                        "CA slot exists without its private key",
                    ));
                }
                let mut options = GenerateKeyOptions::default();
                options
                    .set_key_type(KeyType::ec())
                    .set_size_in_bits(256)
                    .set_label(&label)
                    .set_location(Location::DefaultFileKeychain);
                SecKey::new(&options)?
            }
        };
        let public = public_key_bytes(&key)?;
        let existing = certificate_for_key(&key)?;
        let key = KeychainKey { key, public };
        let certificate = match existing {
            Some(certificate) => certificate,
            None if !create => {
                return Err(Error::InvalidAuthority(
                    "recorded CA certificate is missing",
                ));
            }
            None => {
                let cert = authority::parameters().self_signed(&key)?;
                let certificate = SecCertificate::from_der(cert.der())?;
                ItemAddOptions::new(ItemAddValue::Ref(AddRef::Certificate(certificate.clone())))
                    .set_location(Location::DefaultFileKeychain)
                    .add()?;
                certificate
            }
        };
        let der = certificate.to_der();
        let public = PublicAuthority::try_from(der.clone())?;
        let issuer = Issuer::from_ca_cert_der(&der.into(), key)?;
        Ok(Self {
            issuer,
            certificate,
            public,
        })
    }

    /// Signs a verified CSR after the caller authorizes its DNS name against current host ownership.
    pub fn sign(&self, csr: &VerifiedCertificateRequest) -> Result<Vec<u8>, Error> {
        request::sign_request(
            csr,
            &self.issuer,
            &self.public,
            time::OffsetDateTime::now_utc(),
        )
    }

    /// Exports only the public root certificate for client trust.
    #[must_use]
    pub fn certificate_pem(&self) -> String {
        pem::encode(&pem::Pem::new("CERTIFICATE", self.certificate.to_der()))
    }

    /// Finds this profile's existing public certificate without creating or signing anything.
    pub fn existing_certificate(tls_dir: &Path) -> Result<Option<Vec<u8>>, Error> {
        if let Some(journal) = Self::journal(tls_dir)? {
            return Ok(Some(
                Self::load_root(tls_dir, journal.current())?
                    .certificate
                    .to_der(),
            ));
        }
        Self::slot_certificate(tls_dir, Slot::Primary)
    }

    /// Returns every recorded root for explicit revocation, including unfinished rotation.
    /// No key or certificate is created, and expired public roots remain available.
    pub fn owned_certificates(tls_dir: &Path) -> Result<Vec<PublicAuthority>, Error> {
        let Some(journal) = Self::journal(tls_dir)? else {
            return Self::slot_certificate(tls_dir, Slot::Primary)?
                .map(PublicAuthority::try_from)
                .transpose()
                .map(|root| root.into_iter().collect());
        };
        let mut roots: Vec<_> = journal
            .owned_roots()
            .map(Root::public)
            .collect::<Result<_, _>>()?;
        if let Journal::Preparing { target_slot, .. } = journal
            && let Some(der) = Self::slot_certificate(tls_dir, target_slot)?
        {
            roots.push(PublicAuthority::try_from(der)?);
        }
        Ok(roots)
    }

    pub(crate) fn journal(tls_dir: &Path) -> Result<Option<Journal>, Error> {
        let journal = Journal::load(tls_dir)?;
        if journal.is_none() && Self::slot_has_keys(tls_dir, Slot::Replacement)? {
            return Err(Error::InvalidAuthority(
                "CA rotation journal is missing while the replacement slot exists",
            ));
        }
        Ok(journal)
    }

    pub(crate) fn load_root(tls_dir: &Path, root: &Root) -> Result<Self, Error> {
        let authority = Self::open_slot(tls_dir, root.slot, false)?;
        if authority.certificate.to_der() != root.der {
            return Err(Error::InvalidAuthority(
                "Keychain CA differs from the recorded certificate",
            ));
        }
        Ok(authority)
    }

    pub(crate) fn root(&self, slot: Slot) -> Root {
        Root {
            slot,
            der: self.certificate.to_der(),
        }
    }

    pub(crate) fn slot_has_keys(tls_dir: &Path, slot: Slot) -> Result<bool, Error> {
        let label = Self::slot_label(tls_dir, slot)?;
        match ItemSearchOptions::new()
            .keychains(&[SecKeychain::default()?])
            .class(ItemClass::key())
            .label(&label)
            .load_refs(true)
            .limit(Limit::All)
            .search()
        {
            Ok(items) => Ok(!items.is_empty()),
            Err(error) if error.code() == errSecItemNotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn slot_certificate(tls_dir: &Path, slot: Slot) -> Result<Option<Vec<u8>>, Error> {
        let label = Self::slot_label(tls_dir, slot)?;
        let certificate = match find_key(&label)? {
            Some(key) => certificate_for_key(&key)?,
            None => certificate_without_private_key(&label)?,
        };
        Ok(certificate.map(|certificate| certificate.to_der()))
    }

    fn slot_label(tls_dir: &Path, slot: Slot) -> Result<String, Error> {
        let base = Self::label(tls_dir)?;
        Ok(match slot {
            Slot::Primary => base,
            Slot::Replacement => format!("{base}:replacement"),
        })
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
}

fn certificate_without_private_key(label: &str) -> Result<Option<SecCertificate>, Error> {
    let Some(public) = find_key_class(label, KeyClass::public())? else {
        return Ok(None);
    };
    let bytes = public
        .external_representation()
        .ok_or(Error::InvalidAuthority("missing stored public key bytes"))?;
    certificate_by_public_key(&public_key_hash(&public)?, bytes.bytes())
}

fn find_key(label: &str) -> Result<Option<SecKey>, Error> {
    find_key_class(label, KeyClass::private())
}

fn find_key_class(label: &str, class: KeyClass) -> Result<Option<SecKey>, Error> {
    match find(
        ItemSearchOptions::new()
            .key_class(class)
            .label(label)
            .load_refs(true),
    )? {
        Some(Reference::Key(key)) => Ok(Some(key)),
        Some(_) => Err(Error::InvalidAuthority("expected a key")),
        None => Ok(None),
    }
}

fn certificate_for_key(key: &SecKey) -> Result<Option<SecCertificate>, Error> {
    certificate_by_public_key(&public_key_hash(key)?, &public_key_bytes(key)?)
}

fn certificate_by_public_key(
    hash: &CFData,
    public: &[u8],
) -> Result<Option<SecCertificate>, Error> {
    match find(
        ItemSearchOptions::new()
            .class(ItemClass::certificate())
            .pub_key_hash(hash.bytes())
            .load_refs(true),
    )? {
        Some(Reference::Certificate(certificate)) => {
            let bytes = certificate
                .public_key()?
                .external_representation()
                .ok_or(Error::InvalidAuthority("missing certificate public key"))?;
            if bytes.bytes() != public {
                return Err(Error::InvalidAuthority("certificate and public key differ"));
            }
            Ok(Some(certificate))
        }
        Some(_) => Err(Error::InvalidAuthority("expected a certificate")),
        None => Ok(None),
    }
}

fn check_certificate_key(certificate: &SecCertificate, key: &SecKey) -> Result<(), Error> {
    let certificate_public = certificate
        .public_key()?
        .external_representation()
        .ok_or(Error::InvalidAuthority("missing certificate public key"))?;
    if certificate_public.bytes() != public_key_bytes(key)? {
        return Err(Error::InvalidAuthority(
            "certificate and private key differ",
        ));
    }
    Ok(())
}

fn public_key_bytes(key: &SecKey) -> Result<Vec<u8>, Error> {
    key.public_key()
        .and_then(|key| key.external_representation())
        .map(|bytes| bytes.to_vec())
        .ok_or(Error::InvalidAuthority("missing EC public key"))
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
    match query
        .keychains(&[SecKeychain::default()?])
        .limit(Limit::All)
        .search()
    {
        Ok(items) => unique_reference(items),
        Err(error) if error.code() == errSecItemNotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn unique_reference(mut items: Vec<SearchResult>) -> Result<Option<Reference>, Error> {
    if items.len() > 1 {
        return Err(Error::InvalidAuthority("ambiguous Keychain CA identity"));
    }
    match items.pop() {
        Some(SearchResult::Ref(reference)) => Ok(Some(reference)),
        Some(_) => Err(Error::InvalidAuthority("expected a keychain reference")),
        None => Ok(None),
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

#[cfg(test)]
mod tests;
