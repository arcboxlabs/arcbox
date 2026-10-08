//! What the domain rules need to know about a container.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use super::http_port::{self, HTTP_PORT, HTTPS_PORT, Pin};
use super::https::PROXY_PORT;
use super::selection::HTTPS_LABEL;
use super::{HttpsBackend, RULE_OWNER, SelectedRoutes};
use crate::iptables::NatRule;

/// A running container, as far as its domain is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerFacts {
    pub(super) id: String,
    pub(super) hostname: String,
    /// Its init process, whose `/proc` entry shows the container's sockets.
    pub(super) pid: u32,
    /// Its network namespace (`SandboxKey`).
    pub(super) netns: PathBuf,
    ips: Vec<Ipv4Addr>,
    pub(super) pin: Option<Pin>,
    pub(super) https_pin: Option<Pin>,
    pub(super) exposed: BTreeSet<u16>,
}

impl ContainerFacts {
    /// Reads them from a container's inspect JSON: `None` when it is not
    /// running or has no IPv4 address (host or no networking).
    #[must_use]
    pub fn from_inspect(inspect: &serde_json::Value) -> Option<Self> {
        let id = inspect.get("Id")?.as_str()?.to_owned();
        let hostname = format!(
            "{}.{}",
            inspect.get("Name")?.as_str()?.trim_start_matches('/'),
            arcbox_constants::dns::LOCAL_DOMAIN
        );
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
        let pin = read_pin(inspect, &id, http_port::LABEL);
        let https_pin = read_pin(inspect, &id, HTTPS_LABEL);
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
            hostname,
            pid,
            netns,
            ips,
            pin,
            https_pin,
            exposed,
        })
    }

    /// The container's IPv4 addresses.
    pub(super) fn ips(&self) -> &[Ipv4Addr] {
        &self.ips
    }

    /// Native HTTPS keeps the application's TLS; terminated HTTPS reaches the proxy.
    pub(super) fn rules(&self, selected: SelectedRoutes) -> Vec<NatRule> {
        let native = match selected.https {
            Some(HttpsBackend::Native(port)) => Some(port),
            _ => None,
        };
        let mut rules = Vec::new();
        for ip in &self.ips {
            let ip = ip.to_string();
            for (incoming, target) in [(HTTP_PORT, selected.http), (HTTPS_PORT, native)] {
                let Some(port) = target.filter(|port| *port != incoming) else {
                    continue;
                };
                let destination = format!("{ip}:{port}");
                rules.push(NatRule::new(
                    RULE_OWNER,
                    &self.id,
                    &["-d", &ip, "-p", "tcp", "--dport", &incoming.to_string()],
                    &["-j", "DNAT", "--to-destination", &destination],
                ));
            }
            if selected.terminated_port().is_some() {
                rules.push(NatRule::new(
                    RULE_OWNER,
                    &self.id,
                    &["-d", &ip, "-p", "tcp", "--dport", &HTTPS_PORT.to_string()],
                    &["-j", "REDIRECT", "--to-ports", &PROXY_PORT.to_string()],
                ));
            }
        }
        rules
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect() -> serde_json::Value {
        serde_json::json!({
            "Id": "abc123",
            "Name": "/web",
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

    fn specs(rules: &[NatRule]) -> Vec<String> {
        rules.iter().map(NatRule::spec).collect()
    }

    #[test]
    fn port_80_of_every_address_routes_to_the_http_port() {
        let facts = ContainerFacts::from_inspect(&inspect()).unwrap();
        assert_eq!(
            specs(&facts.rules(SelectedRoutes {
                http: Some(3000),
                https: None
            })),
            [
                "-d 172.17.0.2 -p tcp --dport 80 -m comment --comment arcbox-domain:abc123 \
                 -j DNAT --to-destination 172.17.0.2:3000",
                "-d 172.18.0.3 -p tcp --dport 80 -m comment --comment arcbox-domain:abc123 \
                 -j DNAT --to-destination 172.18.0.3:3000",
            ]
        );
        assert_eq!(
            facts.rules(SelectedRoutes {
                http: Some(80),
                https: None
            }),
            [],
            "80 needs no rule"
        );
        assert_eq!(facts.rules(SelectedRoutes::default()), []);
    }

    #[test]
    fn port_443_goes_to_the_proxy_whatever_the_http_port() {
        let facts = ContainerFacts::from_inspect(&inspect()).unwrap();
        let redirect = "-d 172.17.0.2 -p tcp --dport 443 -m comment --comment \
                        arcbox-domain:abc123 -j REDIRECT --to-ports 61443";
        let served_on_80 =
            specs(&facts.rules(SelectedRoutes::choose(Some(80), None, &BTreeSet::new())));
        assert_eq!(served_on_80.len(), 2, "one redirect per address");
        assert_eq!(served_on_80[0], redirect);
        let served_on_3000 =
            specs(&facts.rules(SelectedRoutes::choose(Some(3000), None, &BTreeSet::new())));
        assert_eq!(served_on_3000.len(), 4);
        assert!(served_on_3000.contains(&redirect.to_owned()));
        assert!(
            facts
                .rules(SelectedRoutes::choose(None, None, &BTreeSet::new()))
                .is_empty(),
            "no HTTP port, nothing to relay to"
        );
    }

    #[test]
    fn https_only_8443_maps_without_a_proxy_or_http_route() {
        let mut inspect = inspect();
        inspect["Config"]["Labels"][HTTPS_LABEL] = "8443".into();
        let facts = ContainerFacts::from_inspect(&inspect).unwrap();
        let selected = SelectedRoutes::choose(None, facts.https_pin, &BTreeSet::new());
        assert_eq!(
            specs(&facts.rules(selected)),
            [
                "-d 172.17.0.2 -p tcp --dport 443 -m comment --comment arcbox-domain:abc123 -j DNAT --to-destination 172.17.0.2:8443",
                "-d 172.18.0.3 -p tcp --dport 443 -m comment --comment arcbox-domain:abc123 -j DNAT --to-destination 172.18.0.3:8443",
            ]
        );
        assert_eq!(
            facts.rules(SelectedRoutes::choose(
                None,
                Some(Pin::Port(443)),
                &BTreeSet::new()
            )),
            []
        );
    }
}
