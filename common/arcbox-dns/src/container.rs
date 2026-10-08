//! Shared Docker inspect metadata for host and guest name registration.

use std::net::Ipv4Addr;

use serde_json::Value;

/// Container identity, default names, and network addresses from one inspect snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMetadata {
    /// The complete container ID supplied by Docker inspect.
    pub id: String,
    /// The original container name without Docker's leading slash.
    pub name: String,
    compose: Option<(String, String)>,
    ipv4_addresses: Vec<Ipv4Addr>,
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
        let compose = inspect.pointer("/Config/Labels").and_then(|labels| {
            let project = labels
                .get("com.docker.compose.project")?
                .as_str()
                .filter(|project| !project.is_empty())?;
            let service = labels
                .get("com.docker.compose.service")?
                .as_str()
                .filter(|service| !service.is_empty())?;
            Some((project.to_owned(), service.to_owned()))
        });
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
            ipv4_addresses,
        })
    }

    /// Returns the Compose service alias first, then the container name, without a domain suffix.
    /// Names retain their original case and underscores for the registration boundary to validate.
    #[must_use]
    pub fn default_aliases(&self) -> Vec<String> {
        match &self.compose {
            Some((project, service)) => vec![format!("{service}.{project}"), self.name.clone()],
            None => vec![self.name.clone()],
        }
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
