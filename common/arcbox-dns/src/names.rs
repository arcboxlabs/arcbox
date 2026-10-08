//! Canonical name patterns shared by host and guest ownership matching.

use arcbox_constants::dns::Domain;

/// Wildcard routing includes every descendant, but never the base name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamePattern {
    /// One canonical DNS name.
    Exact(Domain),
    /// Every proper descendant of one canonical DNS name.
    Descendants(Domain),
}

impl NamePattern {
    /// Parses one custom `.local` label value. CSV splitting belongs to the caller.
    pub fn parse_custom(value: &str) -> Result<Self, String> {
        let value = value.trim().to_ascii_lowercase();
        let (name, wildcard) = value
            .strip_prefix("*.")
            .map_or((value.as_str(), false), |name| (name, true));
        let domain: Domain = name.parse()?;
        if name.rsplit_once('.').map(|(_, tld)| tld) != Some("local") {
            return Err("custom domain must be below .local".into());
        }
        if wildcard && value.len() > 253 {
            return Err("wildcard domain must not exceed 253 characters".into());
        }
        Ok(if wildcard {
            Self::Descendants(domain)
        } else {
            Self::Exact(domain)
        })
    }

    /// Checks a canonical query name without expanding a wildcard into concrete records.
    #[must_use]
    pub fn matches(&self, name: &Domain) -> bool {
        self.rank(name).is_some()
    }

    /// Exact names outrank wildcards; more specific wildcard bases outrank ancestors.
    pub(crate) fn rank(&self, name: &Domain) -> Option<(bool, usize)> {
        match self {
            Self::Exact(exact) => (name == exact).then_some((true, 0)),
            Self::Descendants(root) => name
                .as_str()
                .strip_suffix(root.as_str())
                .is_some_and(|prefix| prefix.ends_with('.'))
                .then(|| (false, root.as_str().split('.').count())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_patterns_normalize_case_and_outer_whitespace() {
        for (raw, expected) in [
            (
                " API.LOCAL ",
                NamePattern::Exact("api.local".parse().unwrap()),
            ),
            (
                "  *.API.LOCAL ",
                NamePattern::Descendants("api.local".parse().unwrap()),
            ),
        ] {
            assert_eq!(NamePattern::parse_custom(raw).unwrap(), expected);
        }
    }

    #[test]
    fn custom_patterns_reject_invalid_names_and_namespace_wildcards() {
        for invalid in [
            "",
            "local",
            "*.local",
            "api.local.",
            "api.test",
            "api..local",
            "https://api.local",
            "api.local:443",
            "127.0.0.1",
            "a.*.local",
            "**.api.local",
        ] {
            assert!(NamePattern::parse_custom(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn wildcard_bases_leave_room_for_a_descendant_label() {
        let prefix = format!("{}.{}.{}", "a".repeat(63), "b".repeat(63), "c".repeat(63));
        for (last_label_len, accepted) in [(53, true), (54, false)] {
            let base = format!("{prefix}.{}.local", "d".repeat(last_label_len));
            assert!(NamePattern::parse_custom(&base).is_ok());
            assert_eq!(
                NamePattern::parse_custom(&format!("*.{base}")).is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn exact_and_wildcard_matches_preserve_apex_and_label_boundaries() {
        let exact = NamePattern::parse_custom("app.local").unwrap();
        let wildcard = NamePattern::parse_custom("*.app.local").unwrap();
        for (name, exact_match, wildcard_match) in [
            ("app.local", true, false),
            ("x.app.local", false, true),
            ("a.b.app.local", false, true),
            ("notapp.local", false, false),
            ("app.local.example", false, false),
            ("local", false, false),
        ] {
            let name = name.parse().unwrap();
            assert_eq!(exact.matches(&name), exact_match, "{name}");
            assert_eq!(wildcard.matches(&name), wildcard_match, "{name}");
        }
    }

    #[test]
    fn ranking_prefers_exact_then_the_most_specific_wildcard() {
        let name = "x.api.app.local".parse().unwrap();
        let rank = |pattern| {
            NamePattern::parse_custom(pattern)
                .unwrap()
                .rank(&name)
                .unwrap()
        };
        assert!(rank("x.api.app.local") > rank("*.api.app.local"));
        assert!(rank("*.api.app.local") > rank("*.app.local"));
        let non_local = NamePattern::Exact("app.test".parse().unwrap());
        assert!(non_local.matches(&"app.test".parse().unwrap()));
    }
}
