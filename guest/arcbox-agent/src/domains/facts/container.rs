//! Docker inspect input for resource-neutral web endpoint facts.

use arcbox_constants::dns::Domain;
use arcbox_dns::container::ContainerMetadata;

use super::super::http_port::{self, Pin};
use super::super::selection::HTTPS_LABEL;
use super::EndpointFacts;

impl EndpointFacts {
    /// Reads facts from a container's inspect JSON.
    /// Returns `None` when identity, a running process, a network namespace, or an IPv4 address is missing.
    #[must_use]
    pub fn from_inspect(inspect: &serde_json::Value, domain: &Domain) -> Option<Self> {
        let metadata = ContainerMetadata::from_inspect(inspect)?;
        let hostname = format!("{}.{domain}", metadata.name);
        let pid = inspect
            .pointer("/State/Pid")?
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|&pid| pid != 0)?;
        let netns = inspect
            .pointer("/NetworkSettings/SandboxKey")?
            .as_str()
            .filter(|key| !key.is_empty())?
            .into();
        let primary_ip = metadata.primary_ipv4()?;
        let mut ips = metadata.ipv4_addresses().to_vec();
        ips.sort_unstable();
        let pin = read_pin(inspect, &metadata.id, http_port::LABEL);
        let https_pin = read_pin(inspect, &metadata.id, HTTPS_LABEL);
        let exposed = inspect
            .pointer("/Config/ExposedPorts")
            .and_then(|ports| ports.as_object())
            .map(|ports| {
                ports
                    .keys()
                    .filter_map(|spec| spec.strip_suffix("/tcp")?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            id: metadata.id,
            hostname,
            pid,
            netns,
            ips,
            primary_ip,
            pin,
            https_pin,
            exposed,
        })
    }
}

fn read_pin(inspect: &serde_json::Value, id: &str, label: &str) -> Option<Pin> {
    let value = inspect.pointer("/Config/Labels")?.get(label)?.as_str()?;
    match value.parse() {
        Ok(pin) => Some(pin),
        Err(error) => {
            tracing::warn!(container_id = id, label, %error, "invalid port override; using protocol discovery");
            None
        }
    }
}
