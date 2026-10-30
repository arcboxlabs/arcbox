use std::str::FromStr;

use super::LOCAL_DOMAIN;
use crate::cmdline::DNS_DOMAIN_KEY;

/// A validated DNS domain name (e.g. `arcbox.local`).
///
/// Guarantees:
/// - Non-empty, ≤253 chars, lowercase alphanumeric + `.` + `-`
/// - No leading/trailing dots, no consecutive dots
/// - Each label ≤63 chars, no leading/trailing `-` (RFC 1035/1123)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain(String);

impl Domain {
    /// Returns the validated DNS domain.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reads the DNS domain from an ArcBox kernel command line.
    ///
    /// A missing token selects the default domain.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is repeated or its value is invalid.
    pub fn from_kernel_cmdline(cmdline: &str) -> Result<Self, String> {
        let mut values = cmdline
            .split_whitespace()
            .filter_map(|token| token.strip_prefix(DNS_DOMAIN_KEY));
        let domain = values.next();
        if values.next().is_some() {
            return Err(format!("kernel command line repeats '{DNS_DOMAIN_KEY}'"));
        }
        domain.map_or_else(|| Ok(Self::default()), str::parse)
    }
}

impl Default for Domain {
    fn default() -> Self {
        Self(LOCAL_DOMAIN.to_owned())
    }
}

impl FromStr for Domain {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("domain must not be empty".to_string());
        }
        if s.len() > 253 {
            return Err(format!("domain too long ({} > 253 chars)", s.len()));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        {
            return Err(format!(
                "domain '{s}' contains invalid characters (allowed: a-z, 0-9, '.', '-')"
            ));
        }
        if s.starts_with('.') || s.ends_with('.') {
            return Err(format!("domain '{s}' must not start or end with '.'"));
        }
        if s.contains("..") {
            return Err(format!(
                "domain '{s}' contains empty label (consecutive dots)"
            ));
        }

        for label in s.split('.') {
            if label.len() > 63 {
                return Err(format!("domain '{s}' has label exceeding 63 chars"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(format!(
                    "domain '{s}' has label starting or ending with '-'"
                ));
            }
        }

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Domain;

    #[test]
    fn valid_domains() {
        assert!("arcbox.local".parse::<Domain>().is_ok());
        assert!("test.arcbox.local".parse::<Domain>().is_ok());
        assert!("my-domain.local".parse::<Domain>().is_ok());
    }

    #[test]
    fn invalid_domains() {
        assert!("".parse::<Domain>().is_err());
        assert!("UPPER.case".parse::<Domain>().is_err());
        assert!("has space.local".parse::<Domain>().is_err());
        let long = "a".repeat(254);
        assert!(long.parse::<Domain>().is_err());
        assert!(".".parse::<Domain>().is_err());
        assert!("..".parse::<Domain>().is_err());
        assert!(".leading".parse::<Domain>().is_err());
        assert!("trailing.".parse::<Domain>().is_err());
        assert!("empty..label".parse::<Domain>().is_err());
    }

    #[test]
    fn domain_rejects_label_starting_or_ending_with_hyphen() {
        assert!("-bad.local".parse::<Domain>().is_err());
        assert!("bad-.local".parse::<Domain>().is_err());
        assert!("ok.-bad".parse::<Domain>().is_err());
        assert!("ok.bad-".parse::<Domain>().is_err());
    }

    #[test]
    fn domain_rejects_label_exceeding_63_chars() {
        let long_label = "a".repeat(64);
        assert!(format!("{long_label}.local").parse::<Domain>().is_err());

        let ok_label = "a".repeat(63);
        assert!(format!("{ok_label}.local").parse::<Domain>().is_ok());
    }

    #[test]
    fn kernel_cmdline_defaults_only_when_the_token_is_absent() {
        assert_eq!(
            Domain::from_kernel_cmdline("root=/dev/vda quiet").unwrap(),
            Domain::default()
        );
        assert_eq!(
            Domain::from_kernel_cmdline("root=/dev/vda arcbox.dns_domain=dev.arcbox.local quiet")
                .unwrap()
                .as_str(),
            "dev.arcbox.local"
        );
        for cmdline in ["arcbox.dns_domain=", "arcbox.dns_domain=BAD.local"] {
            assert!(Domain::from_kernel_cmdline(cmdline).is_err());
        }
    }

    #[test]
    fn kernel_cmdline_rejects_repeated_domains() {
        for cmdline in [
            "arcbox.dns_domain=arcbox.local arcbox.dns_domain=arcbox.local",
            "arcbox.dns_domain=arcbox.local arcbox.dns_domain=dev.arcbox.local",
        ] {
            assert!(Domain::from_kernel_cmdline(cmdline).is_err());
        }
    }
}
