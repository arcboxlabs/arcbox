//! Classify daemon CA metadata without authorizing trust or probing application traffic.

use arcbox_connect::v1::{
    LocalHttpsStatus,
    local_https_status::{CertificateState, TrustState},
};

use super::{CheckState, HealthCheck};

const NAME: &str = "Local HTTPS certificate";
const INSPECT: &str = "Run `abctl tls status --json` and follow the reported error.";
const ROTATE: &str = "Run `abctl tls rotate` to renew the local CA.";

pub(super) fn check(status: &LocalHttpsStatus) -> HealthCheck {
    let (state, repair) = if status.trust_state == TrustState::Error {
        (CheckState::Warning, Some(INSPECT))
    } else if status.trust_state == TrustState::Unavailable {
        (CheckState::Warning, None)
    } else if status.trust_state != TrustState::Trusted
        && status.trust_state != TrustState::Untrusted
    {
        (CheckState::Unknown, None)
    } else if status.certificate_state == CertificateState::Expired
        || status.certificate_state == CertificateState::RenewalDue
    {
        (CheckState::Warning, Some(ROTATE))
    } else if status.certificate_state == CertificateState::NotYetValid {
        (
            CheckState::Warning,
            Some("Check the Mac's date and time, then run `abctl tls status --json`."),
        )
    } else if status.certificate_state == CertificateState::Missing {
        (
            CheckState::Warning,
            Some("Let ArcBox prepare the local CA, then run `abctl tls status --json`."),
        )
    } else if status.certificate_state != CertificateState::Valid {
        (CheckState::Unknown, None)
    } else if status.trust_state == TrustState::Untrusted {
        (
            CheckState::Warning,
            Some("To enable local HTTPS, run `abctl tls trust` and approve the macOS prompt."),
        )
    } else {
        (CheckState::Pass, None)
    };
    HealthCheck {
        name: NAME,
        status: state,
        detail: if status.message.is_empty() {
            "The daemon has not reported local HTTPS certificate status.".into()
        } else {
            status.message.clone()
        },
        repair,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::doctor::DoctorReport;

    fn status(trust: TrustState, certificate: CertificateState) -> LocalHttpsStatus {
        LocalHttpsStatus {
            trust_state: trust.into(),
            certificate_state: certificate.into(),
            message: "CA inspection result".into(),
            ..Default::default()
        }
    }

    #[test]
    fn optional_authorization_and_renewal_preserve_healthy_runtime_reports() {
        for (trust, certificate, repair) in [
            (TrustState::Untrusted, CertificateState::Valid, "tls trust"),
            (
                TrustState::Trusted,
                CertificateState::RenewalDue,
                "tls rotate",
            ),
            (
                TrustState::Untrusted,
                CertificateState::RenewalDue,
                "tls rotate",
            ),
            (
                TrustState::Untrusted,
                CertificateState::Missing,
                "tls status",
            ),
        ] {
            let report = DoctorReport::new(vec![
                HealthCheck::pass("Docker CLI", "docker ps completed"),
                check(&status(trust, certificate)),
            ]);
            assert!(report.healthy);
            assert_eq!(report.summary.warnings, 1);
            assert_eq!(report.summary.failed, 0);
            assert!(report.table().contains("[WARN] Local HTTPS certificate"));
            assert!(report.table().contains(repair));
            let json = serde_json::to_value(report).unwrap();
            assert_eq!(json["healthy"], true);
            assert_eq!(json["summary"]["warnings"], 1);
            assert_eq!(json["checks"][1]["status"], "warning");
        }
    }

    #[test]
    fn unusable_certificates_report_the_repair_without_failing_docker() {
        for (certificate, repair) in [
            (CertificateState::Expired, "tls rotate"),
            (CertificateState::NotYetValid, "date and time"),
        ] {
            let report = DoctorReport::new(vec![
                HealthCheck::pass("Docker CLI", "docker ps completed"),
                check(&status(TrustState::Untrusted, certificate)),
            ]);
            assert!(report.healthy);
            assert_eq!(report.summary.warnings, 1);
            assert!(report.table().contains("[WARN] Local HTTPS certificate"));
            assert!(report.table().contains(repair));
        }
        let inspection_failure = check(&status(TrustState::Error, CertificateState::Unknown));
        assert_eq!(inspection_failure.status, CheckState::Warning);
        assert_eq!(inspection_failure.detail, "CA inspection result");
        assert_eq!(inspection_failure.repair, Some(INSPECT));
        assert_eq!(
            check(&status(TrustState::Unavailable, CertificateState::Unknown)).status,
            CheckState::Warning
        );
    }

    #[test]
    fn only_completed_trust_inspection_passes() {
        assert_eq!(
            check(&LocalHttpsStatus::default()).status,
            CheckState::Unknown
        );
        for trust in [TrustState::Untrusted, TrustState::Trusted] {
            assert_eq!(
                check(&status(trust, CertificateState::Unknown)).status,
                CheckState::Unknown
            );
        }
        assert_eq!(
            check(&status(TrustState::Trusted, CertificateState::Valid)).status,
            CheckState::Pass
        );
    }

    #[test]
    fn optional_https_warning_does_not_hide_a_runtime_failure() {
        let report = DoctorReport::new(vec![
            HealthCheck::fail("Container route", "not installed", "Restart ArcBox."),
            check(&status(TrustState::Untrusted, CertificateState::Valid)),
        ]);
        assert!(!report.healthy);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.warnings, 1);
        assert!(report.table().contains("[FAIL] Container route"));
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["healthy"], false);
        assert_eq!(json["summary"]["failed"], 1);
    }
}
