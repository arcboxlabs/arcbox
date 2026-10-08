//! Keychain identity cleanup.

use super::*;

impl KeychainAuthority {
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
