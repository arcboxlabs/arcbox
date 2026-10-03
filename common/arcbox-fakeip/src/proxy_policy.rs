//! Operator policy for how guest egress reaches the network.
//!
//! [`ProxyPolicy`] is what an operator writes down (`system`, `none`, or a
//! proxy URL); [`ProxySettings`] pairs it with the exclusion list. The
//! datapath turns the settings into a [`ProxyEnvironment`] at startup via
//! [`ProxySettings::resolve`].
//!
//! The policy exists because the datapath used to inherit the host's system
//! proxy unconditionally, snapshotted at VM boot, with no way to opt out
//! (arcboxlabs/arcbox#315). A guest whose traffic must stay direct, or must
//! go through a proxy the Mac itself does not use, needs the choice to be
//! explicit.

use std::fmt;
use std::str::FromStr;

use crate::proxy_detect::{ProxyConfig, ProxyEnvironment};

/// Where guest TCP and UDP egress is sent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxyPolicy {
    /// Follow the Mac's own proxy configuration (`scutil --proxy`, then the
    /// `HTTP_PROXY`/`HTTPS_PROXY` environment), including its exclusions.
    #[default]
    System,
    /// Connect directly, whatever the Mac is configured to do.
    None,
    /// Send everything through this proxy, ignoring the Mac's configuration.
    Custom(ProxyUrl),
}

impl ProxyPolicy {
    /// The spelling of the `system` policy.
    pub const SYSTEM: &'static str = "system";
    /// The spelling of the `none` policy.
    pub const NONE: &'static str = "none";
}

impl fmt::Display for ProxyPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str(Self::SYSTEM),
            Self::None => f.write_str(Self::NONE),
            Self::Custom(url) => url.fmt(f),
        }
    }
}

impl FromStr for ProxyPolicy {
    type Err = ProxyPolicyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case(Self::SYSTEM) || s.eq_ignore_ascii_case("auto") {
            return Ok(Self::System);
        }
        if s.eq_ignore_ascii_case(Self::NONE) || s.eq_ignore_ascii_case("direct") {
            return Ok(Self::None);
        }
        s.parse().map(Self::Custom)
    }
}

/// A proxy the operator named explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUrl {
    /// The proxy protocol.
    pub scheme: ProxyScheme,
    /// The proxy server.
    pub server: ProxyConfig,
}

/// Proxy protocols the datapath can tunnel through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    /// HTTP `CONNECT`.
    Http,
    /// HTTP `CONNECT` over a proxy reached with TLS. The datapath speaks
    /// plain `CONNECT` today, so this is treated as [`Self::Http`].
    Https,
    /// SOCKS5; the only protocol that also carries UDP.
    Socks5,
}

impl ProxyScheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
        }
    }
}

impl fmt::Display for ProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = &self.server.host;
        if host.contains(':') {
            write!(
                f,
                "{}://[{host}]:{}",
                self.scheme.as_str(),
                self.server.port
            )
        } else {
            write!(f, "{}://{host}:{}", self.scheme.as_str(), self.server.port)
        }
    }
}

impl FromStr for ProxyUrl {
    type Err = ProxyPolicyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| ProxyPolicyError::MissingScheme(s.to_string()))?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5" | "socks5h" | "socks" => ProxyScheme::Socks5,
            other => return Err(ProxyPolicyError::UnsupportedScheme(other.to_string())),
        };
        let rest = rest.trim_end_matches('/');
        if rest.contains('@') {
            return Err(ProxyPolicyError::CredentialsUnsupported);
        }
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| ProxyPolicyError::MissingPort(s.to_string()))?;
        let port: u16 = port
            .parse()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| ProxyPolicyError::InvalidPort(port.to_string()))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return Err(ProxyPolicyError::MissingPort(s.to_string()));
        }
        Ok(Self {
            scheme,
            server: ProxyConfig {
                host: host.to_string(),
                port,
            },
        })
    }
}

/// Why a proxy policy string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyPolicyError {
    /// The value is neither `system`, `none`, nor a URL with a scheme.
    #[error("proxy policy {0:?} is not `system`, `none`, or a proxy URL like `socks5://host:1080`")]
    MissingScheme(String),
    /// The URL scheme is not one the datapath can tunnel through.
    #[error("unsupported proxy scheme {0:?}; use http, https, or socks5")]
    UnsupportedScheme(String),
    /// The URL has no `:port`.
    #[error("proxy URL {0:?} has no port")]
    MissingPort(String),
    /// The port is not a non-zero 16-bit integer.
    #[error("invalid proxy port {0:?}")]
    InvalidPort(String),
    /// The URL carries `user:password@`, which the connectors do not send.
    #[error("proxy credentials are not supported yet; use an unauthenticated proxy")]
    CredentialsUnsupported,
}

/// The complete egress policy: where traffic goes, and what stays direct.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxySettings {
    /// Where guest egress is sent.
    pub policy: ProxyPolicy,
    /// Hosts reached directly regardless of the policy, in `NO_PROXY` form:
    /// exact names, `*.suffix` or `.suffix` wildcards. Added to the Mac's own
    /// exclusions under [`ProxyPolicy::System`]; the whole list under
    /// [`ProxyPolicy::Custom`].
    pub exclude: Vec<String>,
}

impl ProxySettings {
    /// Resolves the policy into the environment the datapath enforces.
    ///
    /// `None` means every flow connects directly, so no proxy plumbing is
    /// installed at all. `System` probes the host once here; the caller owns
    /// when that happens.
    #[must_use]
    pub fn resolve(&self) -> Option<ProxyEnvironment> {
        let mut env = match &self.policy {
            ProxyPolicy::None => return None,
            ProxyPolicy::System => ProxyEnvironment::detect(),
            ProxyPolicy::Custom(url) => {
                let mut env = ProxyEnvironment::default();
                match url.scheme {
                    ProxyScheme::Socks5 => env.socks_proxy = Some(url.server.clone()),
                    ProxyScheme::Https => env.https_proxy = Some(url.server.clone()),
                    ProxyScheme::Http => env.http_proxy = Some(url.server.clone()),
                }
                env
            }
        };
        env.bypass_domains
            .extend(self.exclude.iter().map(|entry| normalize_exclusion(entry)));
        Some(env)
    }
}

/// Maps a `NO_PROXY`-style entry onto the `*.suffix` / exact form
/// `ProxyEnvironment::should_bypass` matches: `.example.com` becomes
/// `*.example.com`; everything else passes through unchanged.
fn normalize_exclusion(entry: &str) -> String {
    let entry = entry.trim();
    match entry.strip_prefix('.') {
        Some(suffix) if !entry.starts_with("*.") => format!("*.{suffix}"),
        _ => entry.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> ProxyUrl {
        s.parse().unwrap()
    }

    #[test]
    fn keywords_parse_case_insensitively_with_aliases() {
        for s in ["system", "System", "auto", "", "  "] {
            assert_eq!(
                s.parse::<ProxyPolicy>().unwrap(),
                ProxyPolicy::System,
                "{s:?}"
            );
        }
        for s in ["none", "NONE", "direct"] {
            assert_eq!(
                s.parse::<ProxyPolicy>().unwrap(),
                ProxyPolicy::None,
                "{s:?}"
            );
        }
    }

    #[test]
    fn urls_parse_and_round_trip() {
        let socks = url("socks5://127.0.0.1:1080");
        assert_eq!(socks.scheme, ProxyScheme::Socks5);
        assert_eq!(socks.server.host, "127.0.0.1");
        assert_eq!(socks.server.port, 1080);
        assert_eq!(socks.to_string(), "socks5://127.0.0.1:1080");

        assert_eq!(url("socks5h://proxy.corp:1080").scheme, ProxyScheme::Socks5);
        assert_eq!(url("HTTP://proxy.corp:3128/").scheme, ProxyScheme::Http);
        assert_eq!(url("https://proxy.corp:3129").scheme, ProxyScheme::Https);

        let v6 = url("socks5://[::1]:1080");
        assert_eq!(v6.server.host, "::1");
        assert_eq!(v6.to_string(), "socks5://[::1]:1080");

        let policy: ProxyPolicy = "socks5://10.0.0.5:1080".parse().unwrap();
        assert_eq!(policy.to_string(), "socks5://10.0.0.5:1080");
    }

    #[test]
    fn rejects_malformed_urls_with_a_specific_reason() {
        assert!(matches!(
            "proxy.corp:3128".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::MissingScheme(_))
        ));
        assert!(matches!(
            "ftp://proxy.corp:21".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            "http://proxy.corp".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::MissingPort(_))
        ));
        assert!(matches!(
            "http://proxy.corp:0".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::InvalidPort(_))
        ));
        assert!(matches!(
            "http://:3128".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::MissingPort(_))
        ));
        assert!(matches!(
            "http://user:pw@proxy.corp:3128".parse::<ProxyPolicy>(),
            Err(ProxyPolicyError::CredentialsUnsupported)
        ));
    }

    #[test]
    fn none_installs_no_proxy_at_all() {
        let settings = ProxySettings {
            policy: ProxyPolicy::None,
            exclude: vec!["ignored.example".into()],
        };
        assert!(settings.resolve().is_none());
    }

    #[test]
    fn custom_ignores_the_host_and_uses_only_the_named_proxy() {
        let settings = ProxySettings {
            policy: "socks5://10.0.0.5:1080".parse().unwrap(),
            exclude: vec![".corp.example".into(), "registry.local".into()],
        };
        let env = settings
            .resolve()
            .expect("a custom proxy is an environment");
        assert!(!env.fake_ip_active);
        assert!(env.http_proxy.is_none());
        assert!(env.https_proxy.is_none());
        assert_eq!(env.socks_proxy.as_ref().unwrap().port, 1080);
        assert!(env.has_usable_proxy());
        assert!(env.should_bypass("api.corp.example"));
        assert!(env.should_bypass("corp.example"));
        assert!(env.should_bypass("registry.local"));
        assert!(!env.should_bypass("example.com"));

        let http = ProxySettings {
            policy: "http://proxy.corp:3128".parse().unwrap(),
            exclude: vec![],
        };
        let env = http.resolve().unwrap();
        assert_eq!(env.http_proxy.as_ref().unwrap().port, 3128);
        assert!(env.socks_proxy.is_none());
    }

    #[test]
    fn exclusions_normalize_to_the_bypass_grammar() {
        assert_eq!(normalize_exclusion(".example.com"), "*.example.com");
        assert_eq!(normalize_exclusion("*.example.com"), "*.example.com");
        assert_eq!(normalize_exclusion(" example.com "), "example.com");
        assert_eq!(normalize_exclusion("10.0.0.1"), "10.0.0.1");
    }
}
