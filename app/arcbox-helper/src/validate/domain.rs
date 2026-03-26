use std::str::FromStr;

/// A validated DNS domain name (e.g. `arcbox.local`).
///
/// Guarantees:
/// - Non-empty, ≤253 chars, lowercase alphanumeric + `.` + `-`
/// - No leading/trailing dots, no consecutive dots
/// - Each label ≤63 chars, no leading/trailing `-` (RFC 1035/1123)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain(String);

impl Domain {
    pub fn as_str(&self) -> &str {
        &self.0
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
