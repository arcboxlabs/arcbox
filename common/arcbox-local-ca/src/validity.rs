//! Certificate validity shared by host status and guest leaf renewal.

use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::{Error, MAX_CERTIFICATE_SIZE};

/// The inclusive validity interval encoded in a certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CertificateValidity {
    /// First instant when the certificate is valid.
    pub not_before: OffsetDateTime,
    /// Last instant when the certificate is valid.
    pub not_after: OffsetDateTime,
}

impl CertificateValidity {
    /// Reads exactly one DER certificate without checking its signature or trust.
    pub fn from_der(der: &[u8]) -> Result<Self, Error> {
        if der.len() > MAX_CERTIFICATE_SIZE {
            return Err(Error::InvalidRequest("certificate exceeds 8192 bytes"));
        }
        let (remaining, certificate) = X509Certificate::from_der(der)
            .map_err(|_| Error::InvalidRequest("invalid DER certificate"))?;
        if !remaining.is_empty() {
            return Err(Error::InvalidRequest("trailing certificate data"));
        }
        Ok(Self::from(certificate.validity()))
    }

    /// Returns whether `now` falls within both certificate validity bounds.
    #[must_use]
    pub fn is_valid_at(&self, now: OffsetDateTime) -> bool {
        self.not_before <= now && now <= self.not_after
    }
}

impl From<&x509_parser::certificate::Validity> for CertificateValidity {
    fn from(validity: &x509_parser::certificate::Validity) -> Self {
        Self {
            not_before: validity.not_before.to_datetime(),
            not_after: validity.not_after.to_datetime(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};
    use time::Duration;

    #[test]
    fn certificate_validity_preserves_bounds_and_rejects_extra_data() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut params = CertificateParams::new(vec!["test.arcbox.local".into()]).unwrap();
        params.not_before = now;
        params.not_after = now + Duration::days(30);
        let certificate = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
        let validity = CertificateValidity::from_der(certificate.der()).unwrap();
        assert_eq!(validity.not_before, now);
        assert_eq!(validity.not_after, now + Duration::days(30));
        assert!(!validity.is_valid_at(now - Duration::seconds(1)));
        assert!(validity.is_valid_at(now));
        assert!(validity.is_valid_at(validity.not_after));
        assert!(!validity.is_valid_at(validity.not_after + Duration::seconds(1)));
        assert!(
            CertificateValidity::from_der(&[certificate.der().as_ref(), &[0]].concat()).is_err()
        );
        assert!(CertificateValidity::from_der(&[]).is_err());
        assert!(CertificateValidity::from_der(&vec![0; MAX_CERTIFICATE_SIZE + 1]).is_err());
    }
}
