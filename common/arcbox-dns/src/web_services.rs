//! Public URLs for validated, currently owned web service aliases.
//!
//! Callers select DNS ownership and installed routes before projection.

use arcbox_constants::dns::{Domain, LOCAL_CA_DNS_SUBTREE};

/// How the installed route serves HTTPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpsMode {
    /// No HTTPS route is available.
    None,
    /// ArcBox terminates TLS with its local CA.
    Managed,
    /// The application's TLS handshake and certificate pass through unchanged.
    Passthrough,
}

/// Concrete aliases that share the same public HTTP and HTTPS availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebServiceUrls {
    /// First alias in this group, used by both URLs.
    pub hostname: String,
    /// Aliases in their original order.
    pub domains: Vec<String>,
    /// HTTP URL without an upstream port, or empty when HTTP is unavailable.
    pub http_url: String,
    /// HTTPS URL without an upstream port, or empty when HTTPS is unavailable.
    pub https_url: String,
    /// HTTPS behavior for this alias group.
    pub https_mode: HttpsMode,
}

impl WebServiceUrls {
    /// Projects selected concrete aliases without discovering or authorizing routes.
    ///
    /// Managed HTTPS applies only to `.local` descendants. Foreign aliases form
    /// a separate HTTP-only group. Groups without an available protocol are omitted.
    #[must_use]
    pub fn project(domains: Vec<Domain>, http: bool, https_mode: HttpsMode) -> Vec<Self> {
        let groups = if https_mode == HttpsMode::Managed {
            let (local, foreign): (Vec<_>, Vec<_>) = domains
                .into_iter()
                .partition(|name| name.as_str().ends_with(LOCAL_CA_DNS_SUBTREE));
            vec![(local, https_mode), (foreign, HttpsMode::None)]
        } else {
            vec![(domains, https_mode)]
        };
        groups
            .into_iter()
            .filter_map(|(domains, mode)| {
                let hostname = domains.first()?.to_string();
                if !http && mode == HttpsMode::None {
                    return None;
                }
                Some(Self {
                    http_url: if http {
                        format!("http://{hostname}")
                    } else {
                        String::new()
                    },
                    https_url: if mode == HttpsMode::None {
                        String::new()
                    } else {
                        format!("https://{hostname}")
                    },
                    hostname,
                    domains: domains.into_iter().map(|name| name.to_string()).collect(),
                    https_mode: mode,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(names: &[&str], http: bool, https: HttpsMode) -> Vec<WebServiceUrls> {
        WebServiceUrls::project(
            names.iter().map(|name| name.parse().unwrap()).collect(),
            http,
            https,
        )
    }

    #[test]
    fn managed_https_respects_label_boundaries_and_alias_order() {
        let services = project(
            &[
                "web.example.com",
                "api.local",
                "local",
                "nested.api.local",
                "web.local.evil",
                "localhost",
            ],
            true,
            HttpsMode::Managed,
        );
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].hostname, "api.local");
        assert_eq!(services[0].domains, ["api.local", "nested.api.local"]);
        assert_eq!(services[0].http_url, "http://api.local");
        assert_eq!(services[0].https_url, "https://api.local");
        assert_eq!(services[0].https_mode, HttpsMode::Managed);
        assert_eq!(services[1].hostname, "web.example.com");
        assert_eq!(
            services[1].domains,
            ["web.example.com", "local", "web.local.evil", "localhost"]
        );
        assert_eq!(services[1].http_url, "http://web.example.com");
        assert_eq!(services[1].https_url, "");
        assert_eq!(services[1].https_mode, HttpsMode::None);
    }

    #[test]
    fn native_tls_and_http_only_preserve_the_complete_alias_group() {
        for (http, mode, http_url, https_url) in [
            (false, HttpsMode::Passthrough, "", "https://web.example.com"),
            (
                true,
                HttpsMode::Passthrough,
                "http://web.example.com",
                "https://web.example.com",
            ),
            (true, HttpsMode::None, "http://web.example.com", ""),
        ] {
            let services = project(&["web.example.com", "api.local"], http, mode);
            assert_eq!(services.len(), 1);
            assert_eq!(services[0].domains, ["web.example.com", "api.local"]);
            assert_eq!(services[0].http_url, http_url);
            assert_eq!(services[0].https_url, https_url);
            assert_eq!(services[0].https_mode, mode);
        }
    }

    #[test]
    fn managed_https_without_http_omits_foreign_aliases() {
        let services = project(&["web.example.com", "api.local"], false, HttpsMode::Managed);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].domains, ["api.local"]);
        assert_eq!(services[0].http_url, "");
        assert_eq!(services[0].https_url, "https://api.local");
        assert_eq!(services[0].https_mode, HttpsMode::Managed);
    }

    #[test]
    fn empty_aliases_and_unavailable_protocols_produce_no_links() {
        for mode in [HttpsMode::None, HttpsMode::Managed, HttpsMode::Passthrough] {
            assert_eq!(project(&[], true, mode), []);
        }
        assert_eq!(project(&["api.local"], false, HttpsMode::None), []);
        assert_eq!(project(&["web.example.com"], false, HttpsMode::Managed), []);
    }
}
