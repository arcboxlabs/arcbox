//! Creating the CA, once, in the data directory.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, GeneralSubtree,
    IsCa, KeyPair, KeyUsagePurpose, NameConstraints,
};
use time::{Duration, OffsetDateTime};

use crate::{Error, LOCAL_DOMAIN};

/// How long a generated CA stays valid.
const LIFETIME: Duration = Duration::days(3650);

/// Makes sure `dir` holds a CA, generating one if either file is missing,
/// and returns whether it did.
///
/// An existing pair is kept as is: users trusted that certificate. The key
/// is written first and both files are replaced atomically, so a crash
/// leaves at most a missing certificate, which the next call replaces
/// together with the key.
pub fn ensure(dir: &Path) -> Result<bool, Error> {
    let cert_path = dir.join(TLS_CA_CERT);
    let key_path = dir.join(TLS_CA_KEY);
    if cert_path.exists() && key_path.exists() {
        return Ok(false);
    }
    let (cert_pem, key_pem) = generate()?;
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    // Written with mode 0600, like every atomic write.
    arcbox_atomic_file::write(&key_path, key_pem.as_bytes())?;
    arcbox_atomic_file::write(&cert_path, cert_pem.as_bytes())?;
    fs::set_permissions(&cert_path, fs::Permissions::from_mode(0o644))
        .map_err(|e| Error::io(&cert_path, e))?;
    Ok(true)
}

/// A fresh CA, as `(certificate PEM, PKCS#8 key PEM)`.
fn generate() -> Result<(String, String), rcgen::Error> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "ArcBox Local CA");
    name.push(DnType::OrganizationName, "ArcBox");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(LOCAL_DOMAIN.to_owned())],
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + LIFETIME;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_ca_is_created_once_with_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        assert!(ensure(&tls).unwrap(), "first call generates");
        assert_eq!(mode(&tls.join(TLS_CA_KEY)), 0o600);
        assert_eq!(mode(&tls.join(TLS_CA_CERT)), 0o644);

        let cert = fs::read(tls.join(TLS_CA_CERT)).unwrap();
        assert!(!ensure(&tls).unwrap(), "an existing CA is kept");
        assert_eq!(fs::read(tls.join(TLS_CA_CERT)).unwrap(), cert);
    }

    #[test]
    fn a_half_written_ca_is_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        ensure(dir.path()).unwrap();
        let key = fs::read(dir.path().join(TLS_CA_KEY)).unwrap();
        fs::remove_file(dir.path().join(TLS_CA_CERT)).unwrap();

        assert!(ensure(dir.path()).unwrap());
        assert_ne!(
            fs::read(dir.path().join(TLS_CA_KEY)).unwrap(),
            key,
            "the key goes with the certificate it signed"
        );
    }
}
