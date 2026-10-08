//! Read-only CA status shared by Desktop diagnostics and the login-session CLI.

use std::{path::Path, time::SystemTime};

use arcbox_constants::paths::guest::TLS_CA_CERT;
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority};
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

/// Public metadata only. Network reachability is reported separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Current native trust and certificate validity.
    pub trust_state: TrustState,
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
                message: "Start ArcBox once to create the local HTTPS certificate.".into(),
                certificate_sha256: String::new(),
                certificate_not_before_unix_seconds: 0,
                certificate_not_after_unix_seconds: 0,
                legacy_user_trust: false,
            });
        };
        let authority = PublicAuthority::try_from(der)?;
        let validity = authority.validity();
        let valid = validity.is_valid_at(SystemTime::now().into());
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
        let trust_state = classify(valid, published, policy);
        let message = if !valid {
            "The local HTTPS certificate is outside its validity period. Rotate the CA."
        } else if legacy_user_trust {
            "Authorize local HTTPS to remove the legacy User trust override."
        } else if !published {
            "Authorize local HTTPS to finish publishing the current certificate."
        } else if !admin_trusted {
            "Local HTTPS needs macOS authorization. Containers can run without it."
        } else {
            "The local HTTPS certificate is trusted."
        };
        Ok(Self {
            trust_state,
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
        use arcbox_connect::v1::local_https_status::TrustState as WireTrustState;
        Self {
            trust_state: match status.trust_state {
                TrustState::Untrusted => WireTrustState::Untrusted,
                TrustState::Trusted => WireTrustState::Trusted,
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
