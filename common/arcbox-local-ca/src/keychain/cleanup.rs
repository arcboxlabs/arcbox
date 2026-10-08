//! Delete exact recorded identities after explicit trust revocation.

use super::*;

impl KeychainAuthority {
    /// Deletes this profile's identities after the caller revokes every owned root's trust.
    pub fn delete(tls_dir: &Path) -> Result<(), Error> {
        let mut journal = Self::journal(tls_dir)?;
        if let Some(Journal::Preparing {
            current,
            target_slot,
        }) = &journal
            && let Some(der) = Self::slot_certificate(tls_dir, *target_slot)?
        {
            let prepared = Journal::Prepared {
                current: current.clone(),
                candidate: Root {
                    slot: *target_slot,
                    der,
                },
            };
            prepared.persist(tls_dir)?;
            journal = Some(prepared);
        }
        let owned = Self::owned_certificates(tls_dir)?;
        for root in &owned {
            require_untrusted(root)?;
        }
        let path = tls_dir.join(arcbox_constants::paths::guest::TLS_CA_CERT);
        match std::fs::read(&path) {
            Ok(pem) => {
                let published = PublicAuthority::from_pem(&pem)?;
                require_untrusted(&published)?;
                crate::migration::delete_certificate_der(published.der())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
        if let Some(journal) = journal {
            for root in journal.owned_roots() {
                Self::delete_root(tls_dir, root)?;
            }
            if let Journal::Preparing { target_slot, .. } = journal {
                Self::delete_slot(tls_dir, target_slot, None)?;
            }
        } else {
            Self::delete_slot(
                tls_dir,
                Slot::Primary,
                owned.first().map(PublicAuthority::der),
            )?;
        }
        for root in &owned {
            require_untrusted(root)?;
        }
        Journal::remove(tls_dir)
    }

    pub(crate) fn delete_root(tls_dir: &Path, root: &Root) -> Result<(), Error> {
        Self::delete_slot(tls_dir, root.slot, Some(&root.der))
    }

    fn delete_slot(tls_dir: &Path, slot: Slot, expected: Option<&[u8]>) -> Result<(), Error> {
        let label = Self::slot_label(tls_dir, slot)?;
        let key = find_key(&label)?;
        let stored = match &key {
            Some(key) => certificate_for_key(key)?,
            None => certificate_without_private_key(&label)?,
        };
        let stored_der = stored.as_ref().map(SecCertificate::to_der);
        if let (Some(expected), Some(stored)) = (expected, stored_der.as_deref())
            && expected != stored
        {
            return Err(Error::InvalidAuthority(
                "cleanup certificate differs from the recorded root",
            ));
        }
        let der = expected.or(stored_der.as_deref());
        let certificate = der.map(SecCertificate::from_der).transpose()?;
        let public_bytes = match (&certificate, &key) {
            (Some(certificate), _) => Some(
                certificate
                    .public_key()?
                    .external_representation()
                    .ok_or(Error::InvalidAuthority("missing certificate public key"))?
                    .to_vec(),
            ),
            (None, Some(key)) => Some(public_key_bytes(key)?),
            (None, None) => None,
        };
        let public_key = find_key_class(&label, KeyClass::public())?;
        if let Some(public_key) = &public_key {
            let recorded = public_bytes.as_deref().ok_or(Error::InvalidAuthority(
                "CA public key has no recorded certificate or private key",
            ))?;
            let bytes = public_key
                .external_representation()
                .ok_or(Error::InvalidAuthority("missing stored public key bytes"))?;
            if bytes.bytes() != recorded {
                return Err(Error::InvalidAuthority(
                    "cleanup public key differs from the recorded root",
                ));
            }
        }
        if let Some(der) = der {
            require_untrusted(&PublicAuthority::try_from(der.to_vec())?)?;
            if let Some(key) = &key {
                check_certificate_key(&SecCertificate::from_der(der)?, key)?;
            }
        }
        // Delete the public key first so partial cleanup retains its identity source.
        if let Some(public_key) = public_key {
            public_key.delete()?;
        }
        if let Some(der) = der {
            crate::migration::delete_certificate_der(der)?;
        }
        if let Some(key) = key {
            key.delete()?;
        }
        Ok(())
    }
}

fn require_untrusted(root: &PublicAuthority) -> Result<(), Error> {
    if root.has_trust()? {
        return Err(Error::InvalidAuthority(
            "revoke the root's User and Admin trust before deleting its identity",
        ));
    }
    Ok(())
}
