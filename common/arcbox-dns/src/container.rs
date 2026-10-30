//! Shared Docker inspect metadata for host and guest name registration.

use std::net::Ipv4Addr;

use arcbox_constants::dns::Domain;
use serde_json::Value;

use crate::{NamePattern, claims::Claim};

const CUSTOM_DOMAINS_LABEL: &str = "dev.arcbox.domains";

/// Container identity, default names, and network addresses from one inspect snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMetadata {
    /// The complete container ID supplied by Docker inspect.
    pub id: String,
    /// The original container name without Docker's leading slash.
    pub name: String,
    compose: Option<(String, String)>,
    custom_domains: Vec<NamePattern>,
    warnings: Vec<String>,
    ipv4_addresses: Vec<Ipv4Addr>,
}

/// Canonical name claims and diagnostics for individually rejected names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerClaims {
    /// Valid claims with the container's complete ID and caller-supplied authority.
    pub claims: Vec<Claim>,
    /// Rejected names and their validation errors for the caller to report.
    pub warnings: Vec<String>,
}

impl ContainerMetadata {
    /// Reads an inspect snapshot with a nonempty ID and container name.
    /// Missing networks or IPv4 addresses do not discard the container identity.
    #[must_use]
    pub fn from_inspect(inspect: &Value) -> Option<Self> {
        let id = inspect.get("Id")?.as_str().filter(|id| !id.is_empty())?;
        let name = inspect.get("Name")?.as_str()?.trim_start_matches('/');
        if name.is_empty() {
            return None;
        }
        let labels = inspect.pointer("/Config/Labels").and_then(Value::as_object);
        let label = |key: &str| labels?.get(key)?.as_str();
        let oneoff = label("com.docker.compose.oneoff")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let compose = label("com.docker.compose.project")
            .zip(label("com.docker.compose.service"))
            .filter(|(project, service)| !oneoff && !project.is_empty() && !service.is_empty())
            .map(|(project, service)| (project.to_owned(), service.to_owned()));
        let mut custom_domains = Vec::new();
        let mut warnings = Vec::new();
        if !oneoff && let Some(domains) = label(CUSTOM_DOMAINS_LABEL) {
            for value in domains.split(',') {
                match NamePattern::parse_custom(value) {
                    Ok(pattern) => custom_domains.push(pattern),
                    Err(error) => warnings.push(format!(
                        "{CUSTOM_DOMAINS_LABEL} entry {value:?} rejected: {error}"
                    )),
                }
            }
        }
        let mut networks: Vec<_> = inspect
            .pointer("/NetworkSettings/Networks")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|networks| networks.iter())
            .collect();
        networks.sort_unstable_by_key(|(name, _)| *name);
        let mut ipv4_addresses = Vec::new();
        for (_, network) in networks {
            let Some(address) = network
                .get("IPAddress")
                .and_then(Value::as_str)
                .and_then(|address| address.parse().ok())
            else {
                continue;
            };
            if !ipv4_addresses.contains(&address) {
                ipv4_addresses.push(address);
            }
        }
        Some(Self {
            id: id.to_owned(),
            name: name.to_owned(),
            compose,
            custom_domains,
            warnings,
            ipv4_addresses,
        })
    }

    /// Returns the Compose service alias first, then the container name, without a domain suffix.
    /// Names retain their original case and underscores for the registration boundary to validate.
    /// One-off containers return only their unique container name.
    #[must_use]
    pub fn default_aliases(&self) -> Vec<String> {
        match &self.compose {
            Some((project, service)) => vec![format!("{service}.{project}"), self.name.clone()],
            None => vec![self.name.clone()],
        }
    }

    /// Produces canonical default and custom claims under a trusted registry authority.
    ///
    /// Invalid names produce warnings without suppressing other valid claims.
    /// One-off containers claim only their unique name and its possible descendants.
    /// Default names of 252 or 253 bytes remain exact claims with no descendant wildcard.
    #[must_use]
    pub fn claims(&self, suffix: &Domain, authority: &str) -> ContainerClaims {
        let mut result = ContainerClaims {
            claims: Vec::new(),
            warnings: Vec::new(),
        };
        let mut generated = |alias: &str, group: Option<&(String, String)>| {
            let name = format!("{alias}.{suffix}").to_ascii_lowercase();
            let domain = match name.parse::<Domain>() {
                Ok(domain) => domain,
                Err(error) => {
                    let source = if group.is_some() {
                        "Compose service"
                    } else {
                        "container name"
                    };
                    result
                        .warnings
                        .push(format!("{source} {alias:?} rejected: {error}"));
                    return;
                }
            };
            // A descendant needs at least one label and a dot within the 253-byte limit.
            let descendants_fit = domain.as_str().len() <= 251;
            let [exact, descendants] = match group {
                Some(group) => Claim::compose_service(
                    domain,
                    self.id.clone(),
                    authority.to_owned(),
                    group.clone(),
                ),
                None => Claim::generated(domain, self.id.clone(), authority.to_owned()),
            };
            result.claims.push(exact);
            if descendants_fit {
                result.claims.push(descendants);
            }
        };
        if let Some(group) = &self.compose {
            generated(&format!("{}.{}", group.1, group.0), Some(group));
        }
        generated(&self.name, None);
        result.claims.extend(
            self.custom_domains
                .iter()
                .cloned()
                .map(|pattern| Claim::exclusive(pattern, self.id.clone(), authority.to_owned())),
        );
        result.warnings.extend(self.warnings.iter().cloned());
        result
    }

    /// Returns the first valid IPv4 address in lexicographic network-name order.
    #[must_use]
    pub fn primary_ipv4(&self) -> Option<Ipv4Addr> {
        self.ipv4_addresses.first().copied()
    }

    /// Returns distinct IPv4 addresses in network-name order, with the primary address first.
    #[must_use]
    pub fn ipv4_addresses(&self) -> &[Ipv4Addr] {
        &self.ipv4_addresses
    }
}
