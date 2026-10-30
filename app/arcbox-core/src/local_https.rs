//! Read-only CA status shared by Desktop diagnostics and the login-session CLI.

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use arcbox_constants::paths::guest::TLS_CA_CERT;
use arcbox_helper::client::Client;
use arcbox_local_ca::{CertificateValidity, KeychainAuthority, PublicAuthority};
use serde::Serialize;

use crate::Result;

/// Whether the profile's current certificate is ready for system TLS trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustState {
    /// Native authorization, publication, or certificate renewal is required.
    Untrusted,
    /// The current, valid certificate has precise Admin trust and no User override.
    Trusted,
}

/// Certificate lifetime, independent of publication and native trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateState {
    /// This profile has no Keychain certificate.
    Missing,
    /// The certificate is valid for more than 30 days.
    Valid,
    /// The certificate is valid and expires within 30 days.
    RenewalDue,
    /// The certificate's validity period has ended.
    Expired,
    /// The certificate's validity period has not started.
    NotYetValid,
}

impl CertificateState {
    fn at(validity: CertificateValidity, now: SystemTime) -> Self {
        const RENEWAL_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);
        if now < SystemTime::from(validity.not_before) {
            Self::NotYetValid
        } else if now > SystemTime::from(validity.not_after) {
            Self::Expired
        } else if SystemTime::from(validity.not_after) <= now + RENEWAL_WINDOW {
            Self::RenewalDue
        } else {
            Self::Valid
        }
    }

    fn is_valid(self) -> bool {
        matches!(self, Self::Valid | Self::RenewalDue)
    }
}

/// Public metadata only. Network reachability is reported separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Current native trust and certificate validity.
    pub trust_state: TrustState,
    /// Current certificate lifetime without implying native trust.
    pub certificate_state: CertificateState,
    /// A user-facing explanation of the current state.
    pub message: String,
    /// Uppercase, colon-separated SHA-256 fingerprint.
    pub certificate_sha256: String,
    /// Inclusive validity start in UTC Unix seconds; zero without a certificate.
    pub certificate_not_before_unix_seconds: i64,
    /// Inclusive validity end in UTC Unix seconds; zero without a certificate.
    pub certificate_not_after_unix_seconds: i64,
    /// User trust overrides the required Admin policy until migration removes it.
    pub legacy_user_trust: bool,
}

impl Status {
    /// Inspects the profile without creating a key, publishing a file, or changing trust.
    pub async fn inspect(tls_dir: &Path) -> Result<Self> {
        let Some(der) = KeychainAuthority::existing_certificate(tls_dir)? else {
            return Ok(Self {
                trust_state: TrustState::Untrusted,
                certificate_state: CertificateState::Missing,
                message: "Start ArcBox once to create the local HTTPS certificate.".into(),
                certificate_sha256: String::new(),
                certificate_not_before_unix_seconds: 0,
                certificate_not_after_unix_seconds: 0,
                legacy_user_trust: false,
            });
        };
        let authority = PublicAuthority::try_from(der)?;
        let validity = authority.validity();
        let certificate_state = CertificateState::at(validity, SystemTime::now());
        let published = match std::fs::read(tls_dir.join(TLS_CA_CERT)) {
            Ok(pem) => {
                PublicAuthority::from_pem(&pem)
                    .map_err(|error| {
                        crate::CoreError::invalid_state(format!(
                            "invalid public CA file: {error}; run `abctl tls repair` to check whether the public file can be restored"
                        ))
                    })?
                    .der()
                    == authority.der()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let legacy_user_trust = authority.has_user_trust()?;
        let admin_trusted = Client::connect().await?.ca_status(authority.der()).await?;
        let policy = match (legacy_user_trust, admin_trusted) {
            (true, _) => NativeTrust::UserOverride,
            (false, true) => NativeTrust::TlsOnlyAdmin,
            (false, false) => NativeTrust::Absent,
        };
        let trust_state = classify(certificate_state.is_valid(), published, policy);
        let message = if certificate_state == CertificateState::NotYetValid {
            "The local HTTPS certificate is not valid yet. Check the Mac's date and time."
        } else if certificate_state == CertificateState::Expired {
            "The local HTTPS certificate has expired. Renew the CA to restore HTTPS."
        } else if legacy_user_trust {
            "Authorize local HTTPS to remove the legacy User trust override."
        } else if !published {
            "Authorize local HTTPS to finish publishing the current certificate."
        } else if !admin_trusted {
            "Local HTTPS needs macOS authorization. Containers can run without it."
        } else if certificate_state == CertificateState::RenewalDue {
            "The local HTTPS certificate is trusted and expires within 30 days."
        } else {
            "The local HTTPS certificate is trusted."
        };
        Ok(Self {
            trust_state,
            certificate_state,
            message: message.into(),
            certificate_sha256: authority.fingerprint_sha256(),
            certificate_not_before_unix_seconds: validity.not_before.unix_timestamp(),
            certificate_not_after_unix_seconds: validity.not_after.unix_timestamp(),
            legacy_user_trust,
        })
    }
}

impl From<Status> for arcbox_connect::v1::LocalHttpsStatus {
    fn from(status: Status) -> Self {
        use arcbox_connect::v1::local_https_status::CertificateState as WireCertificateState;
        use arcbox_connect::v1::local_https_status::TrustState as WireTrustState;
        Self {
            trust_state: match status.trust_state {
                TrustState::Untrusted => WireTrustState::Untrusted,
                TrustState::Trusted => WireTrustState::Trusted,
            }
            .into(),
            certificate_state: match status.certificate_state {
                CertificateState::Missing => WireCertificateState::Missing,
                CertificateState::Valid => WireCertificateState::Valid,
                CertificateState::RenewalDue => WireCertificateState::RenewalDue,
                CertificateState::Expired => WireCertificateState::Expired,
                CertificateState::NotYetValid => WireCertificateState::NotYetValid,
            }
            .into(),
            message: status.message,
            certificate_sha256: status.certificate_sha256,
            certificate_not_before_unix_seconds: status.certificate_not_before_unix_seconds,
            certificate_not_after_unix_seconds: status.certificate_not_after_unix_seconds,
            legacy_user_trust: status.legacy_user_trust,
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy)]
enum NativeTrust {
    Absent,
    TlsOnlyAdmin,
    UserOverride,
}

fn classify(valid: bool, published: bool, policy: NativeTrust) -> TrustState {
    if valid && published && matches!(policy, NativeTrust::TlsOnlyAdmin) {
        TrustState::Trusted
    } else {
        TrustState::Untrusted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_lifetime_preserves_validity_and_thirty_day_renewal_boundaries() {
        let begins = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let expires = begins + Duration::from_secs(90 * 24 * 60 * 60);
        let renewal = expires - Duration::from_secs(30 * 24 * 60 * 60);
        let validity = CertificateValidity {
            not_before: begins.into(),
            not_after: expires.into(),
        };
        for (now, expected, trusted) in [
            (
                begins - Duration::from_secs(1),
                CertificateState::NotYetValid,
                false,
            ),
            (begins, CertificateState::Valid, true),
            (
                renewal - Duration::from_secs(1),
                CertificateState::Valid,
                true,
            ),
            (renewal, CertificateState::RenewalDue, true),
            (expires, CertificateState::RenewalDue, true),
            (
                expires + Duration::from_secs(1),
                CertificateState::Expired,
                false,
            ),
        ] {
            let state = CertificateState::at(validity, now);
            assert_eq!(state, expected);
            assert_eq!(
                classify(state.is_valid(), true, NativeTrust::TlsOnlyAdmin) == TrustState::Trusted,
                trusted,
            );
        }
    }

    #[test]
    fn stale_publication_expiry_or_user_override_prevents_ready_trust() {
        assert_eq!(
            classify(true, true, NativeTrust::TlsOnlyAdmin),
            TrustState::Trusted
        );
        for facts in [
            (false, true, NativeTrust::TlsOnlyAdmin),
            (true, false, NativeTrust::TlsOnlyAdmin),
            (true, true, NativeTrust::Absent),
            (true, true, NativeTrust::UserOverride),
        ] {
            assert_eq!(classify(facts.0, facts.1, facts.2), TrustState::Untrusted);
        }
    }
}
