//! What the domain rules need to know about a container.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use super::RULE_OWNER;
use super::http_port::{self, HTTP_PORT, Pin};
use crate::iptables::NatRule;

/// A running container, as far as its domain is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerFacts {
    pub(super) id: String,
    /// Its init process, whose `/proc` entry shows the container's sockets.
    pub(super) pid: u32,
    /// Its network namespace (`SandboxKey`).
    pub(super) netns: PathBuf,
    ips: Vec<Ipv4Addr>,
    pub(super) pin: Option<Pin>,
    pub(super) exposed: BTreeSet<u16>,
}

impl ContainerFacts {
    /// Reads them from a container's inspect JSON: `None` when it is not
    /// running or has no IPv4 address (host or no networking).
    #[must_use]
    pub fn from_inspect(inspect: &serde_json::Value) -> Option<Self> {
        let id = inspect.get("Id")?.as_str()?.to_owned();
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
        let mut ips: Vec<Ipv4Addr> = inspect
            .pointer("/NetworkSettings/Networks")?
            .as_object()?
            .values()
            .filter_map(|net| net.get("IPAddress")?.as_str()?.parse().ok())
            .collect();
        ips.sort_unstable();
        ips.dedup();
        if ips.is_empty() {
            return None;
        }
        let pin = inspect
            .pointer("/Config/Labels")
            .and_then(|labels| labels.get(http_port::LABEL)?.as_str())
            .and_then(|value| match value.parse() {
                Ok(pin) => Some(pin),
                Err(e) => {
                    tracing::warn!(container_id = %id, "{e}; choosing the HTTP port by itself");
                    None
                }
            });
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
            id,
            pid,
            netns,
            ips,
            pin,
            exposed,
        })
    }

    /// The rules that route port 80 of each of the container's addresses
    /// to `http_port`; none when that is 80 itself or there is none.
    pub(super) fn rules(&self, http_port: Option<u16>) -> Vec<NatRule> {
        let Some(port) = http_port.filter(|&port| port != HTTP_PORT) else {
            return Vec::new();
        };
        let dport = HTTP_PORT.to_string();
        self.ips
            .iter()
            .map(|ip| {
                let ip = ip.to_string();
                let destination = format!("{ip}:{port}");
                NatRule::new(
                    RULE_OWNER,
                    &self.id,
                    &["-d", &ip, "-p", "tcp", "--dport", &dport],
                    &["-j", "DNAT", "--to-destination", &destination],
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect() -> serde_json::Value {
        serde_json::json!({
            "Id": "abc123",
            "State": { "Pid": 4242 },
            "Config": {
                "Labels": { "com.example": "x" },
                "ExposedPorts": { "8080/tcp": {}, "53/udp": {} }
            },
            "NetworkSettings": {
                "SandboxKey": "/var/run/docker/netns/f6f5a3c09806",
                "Networks": {
                    "bridge": { "IPAddress": "172.17.0.2" },
                    "app": { "IPAddress": "172.18.0.3" },
                    "none": { "IPAddress": "" }
                }
            }
        })
    }

    #[test]
    fn facts_come_from_inspect() {
        let facts = ContainerFacts::from_inspect(&inspect()).unwrap();
        assert_eq!(facts.id, "abc123");
        assert_eq!(facts.pid, 4242);
        assert_eq!(
            facts.netns,
            PathBuf::from("/var/run/docker/netns/f6f5a3c09806")
        );
        assert_eq!(
            facts.ips,
            [
                "172.17.0.2".parse::<Ipv4Addr>().unwrap(),
                "172.18.0.3".parse().unwrap()
            ]
        );
        assert_eq!(facts.pin, None);
        assert_eq!(facts.exposed, BTreeSet::from([8080]));
    }

    #[test]
    fn stopped_or_unaddressed_containers_have_no_facts() {
        let mut stopped = inspect();
        stopped["State"]["Pid"] = 0.into();
        assert_eq!(ContainerFacts::from_inspect(&stopped), None);

        let mut no_sandbox = inspect();
        no_sandbox["NetworkSettings"]["SandboxKey"] = "".into();
        assert_eq!(ContainerFacts::from_inspect(&no_sandbox), None);

        let mut host = inspect();
        host["NetworkSettings"]["Networks"] = serde_json::json!({ "host": { "IPAddress": "" } });
        assert_eq!(ContainerFacts::from_inspect(&host), None);
    }

    #[test]
    fn the_label_is_read_and_a_bad_one_ignored() {
        let mut pinned = inspect();
        pinned["Config"]["Labels"][http_port::LABEL] = "off".into();
        assert_eq!(
            ContainerFacts::from_inspect(&pinned).unwrap().pin,
            Some(Pin::Off)
        );

        pinned["Config"]["Labels"][http_port::LABEL] = "web".into();
        assert_eq!(ContainerFacts::from_inspect(&pinned).unwrap().pin, None);
    }

    #[test]
    fn port_80_of_every_address_routes_to_the_http_port() {
        let facts = ContainerFacts::from_inspect(&inspect()).unwrap();
        let specs: Vec<String> = facts.rules(Some(3000)).iter().map(NatRule::spec).collect();
        assert_eq!(
            specs,
            [
                "-d 172.17.0.2 -p tcp --dport 80 -m comment --comment arcbox-domain:abc123 \
                 -j DNAT --to-destination 172.17.0.2:3000",
                "-d 172.18.0.3 -p tcp --dport 80 -m comment --comment arcbox-domain:abc123 \
                 -j DNAT --to-destination 172.18.0.3:3000",
            ]
        );
        assert!(facts.rules(Some(80)).is_empty(), "80 needs no rule");
        assert!(facts.rules(None).is_empty());
    }
}
