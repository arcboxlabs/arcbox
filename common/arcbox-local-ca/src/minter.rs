//! Minting a certificate per server name, on demand, from the CA.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::KeyProvider;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::{Duration, OffsetDateTime};

use crate::{Error, LOCAL_DOMAIN};

/// How long a minted certificate is valid.
const LIFETIME: Duration = Duration::days(30);

/// How long before expiry a cached certificate is replaced.
const RENEW_BEFORE: Duration = Duration::days(7);

/// How far back a certificate's validity starts, for clocks that disagree.
const BACKDATE: Duration = Duration::hours(1);

/// Server names cached at once; the cache starts over past it.
const MAX_CACHED: usize = 256;

/// Signs a certificate for each server name a TLS client asks for.
pub struct LeafMinter {
    issuer: Issuer<'static, KeyPair>,
    keys: &'static dyn KeyProvider,
    cache: Mutex<HashMap<String, Leaf>>,
}

struct Leaf {
    key: Arc<CertifiedKey>,
    valid_from: OffsetDateTime,
    renew_at: OffsetDateTime,
}

impl LeafMinter {
    /// Loads the CA the daemon wrote into `dir`.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))
        };
        let key = KeyPair::from_pem(&read(TLS_CA_KEY)?)?;
        let issuer = Issuer::from_ca_cert_pem(&read(TLS_CA_CERT)?, key)?;
        Ok(Self {
            issuer,
            keys: rustls::crypto::ring::default_provider().key_provider,
            cache: Mutex::default(),
        })
    }

    /// A TLS server configuration that presents this minter's certificates.
    pub fn into_server_config(self) -> Result<rustls::ServerConfig, Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Ok(rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(self)))
    }

    /// The certificate for `name`, reused while it is fresh.
    pub fn certificate(&self, name: &str) -> Result<Arc<CertifiedKey>, Error> {
        self.certificate_at(name, OffsetDateTime::now_utc())
    }

    fn certificate_at(&self, name: &str, now: OffsetDateTime) -> Result<Arc<CertifiedKey>, Error> {
        let name = name.to_ascii_lowercase();
        let host = name
            .strip_suffix(LOCAL_DOMAIN)
            .and_then(|host| host.strip_suffix('.'));
        if host.is_none_or(str::is_empty) {
            return Err(Error::ForeignName(name));
        }
        // A certificate minted under a clock that has since moved (the
        // guest clock is set from the host after boot) is not fresh either.
        if let Some(leaf) = self.cache().get(&name) {
            if leaf.valid_from <= now && now < leaf.renew_at {
                return Ok(Arc::clone(&leaf.key));
            }
        }
        let leaf = self.mint(&name, now)?;
        let key = Arc::clone(&leaf.key);
        let mut cache = self.cache();
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(name, leaf);
        Ok(key)
    }

    fn mint(&self, name: &str, now: OffsetDateTime) -> Result<Leaf, Error> {
        let mut params = CertificateParams::new(vec![name.to_owned()])?;
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, name);
        params.distinguished_name = subject;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.not_before = now - BACKDATE;
        params.not_after = now + LIFETIME;
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.issuer)?;
        let signing_key =
            self.keys
                .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    key.serialize_der(),
                )))?;
        Ok(Leaf {
            key: Arc::new(CertifiedKey::new(vec![cert.der().clone()], signing_key)),
            valid_from: params.not_before,
            renew_at: params.not_after - RENEW_BEFORE,
        })
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, Leaf>> {
        self.cache.lock().expect("leaf cache lock poisoned")
    }
}

impl ResolvesServerCert for LeafMinter {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?;
        match self.certificate(name) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::debug!(server_name = name, error = %e, "no certificate for TLS client");
                None
            }
        }
    }
}

impl fmt::Debug for LeafMinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeafMinter")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use rustls::RootCertStore;
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier as _;
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tempfile::TempDir;

    use super::*;

    fn authority() -> (TempDir, LeafMinter) {
        let dir = tempfile::tempdir().unwrap();
        crate::ensure(dir.path()).unwrap();
        let minter = LeafMinter::load(dir.path()).unwrap();
        (dir, minter)
    }

    /// Verifies `leaf` for `name` the way a TLS client trusting only the CA does.
    fn verify(dir: &Path, leaf: &CertificateDer<'_>, name: &str) -> Result<(), rustls::Error> {
        let ca = CertificateDer::from_pem_file(dir.join(TLS_CA_CERT)).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        let name = ServerName::try_from(name.to_owned()).unwrap();
        verifier
            .verify_server_cert(leaf, &[], &name, &[], UnixTime::now())
            .map(|_| ())
    }

    #[test]
    fn a_minted_certificate_chains_to_the_ca_for_its_name() {
        let (dir, minter) = authority();
        let key = minter.certificate("Web.arcbox.local").unwrap();
        verify(dir.path(), &key.cert[0], "web.arcbox.local").unwrap();
        assert!(
            verify(dir.path(), &key.cert[0], "api.arcbox.local").is_err(),
            "a certificate names one host"
        );
        let service = minter.certificate("web.shop.arcbox.local").unwrap();
        verify(dir.path(), &service.cert[0], "web.shop.arcbox.local").unwrap();
    }

    #[test]
    fn names_outside_the_domain_get_nothing() {
        let (_dir, minter) = authority();
        for name in [
            "example.com",
            "arcbox.local",
            "evilarcbox.local",
            "web.arcbox.local.example.com",
        ] {
            assert!(
                matches!(minter.certificate(name), Err(Error::ForeignName(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn the_ca_vouches_for_no_other_name_whatever_its_key_signs() {
        // What a leaked key would sign: the CA's name constraints are what
        // keep it from verifying.
        let (dir, minter) = authority();
        let forged = minter
            .mint("example.com", OffsetDateTime::now_utc())
            .unwrap();
        let err = verify(dir.path(), &forged.key.cert[0], "example.com").unwrap_err();
        assert!(
            format!("{err:?}").contains("NameConstraintViolation"),
            "{err:?}"
        );
    }

    #[test]
    fn certificates_are_reused_until_they_near_expiry() {
        let (_dir, minter) = authority();
        let now = OffsetDateTime::now_utc();
        let name = "web.arcbox.local";
        let first = minter.certificate_at(name, now).unwrap();
        let later = minter
            .certificate_at(name, now + Duration::days(1))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &later));

        let renewed = minter
            .certificate_at(name, now + LIFETIME - RENEW_BEFORE)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &renewed));

        let earlier = minter
            .certificate_at(name, now - Duration::days(365))
            .unwrap();
        assert!(
            !Arc::ptr_eq(&renewed, &earlier),
            "a clock set back after minting gets a certificate valid for it"
        );
    }
}
