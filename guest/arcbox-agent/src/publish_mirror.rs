//! Reaching containers published to a specific host address.
//!
//! `docker run -p 127.0.0.1:8080:80` asks dockerd to accept connections to
//! that host address only, and dockerd installs a DNAT rule with a
//! matching `-d 127.0.0.1` in the guest. ArcBox's host-side listener honours
//! the address on the Mac, but the inbound relay then dials the container
//! port at the guest's *uplink* address, which that rule does not match:
//! the guest answers with a RST and every `-p 127.0.0.1:…` (or any other
//! explicit IPv4 host address) publish is unreachable from the host, while
//! `0.0.0.0` bindings work because their rule carries no `-d`.
//!
//! The agent therefore mirrors each such binding with a rule that matches
//! traffic arriving on the uplink interface for the same host port, DNATing
//! it to the same container endpoint dockerd chose. The rule lives only in
//! the guest, carries a comment naming the container, and is removed when
//! the container dies; rules left behind by an agent restart are swept at
//! startup. Since the uplink only ever carries traffic the host relay
//! injected, and the relay only injects what its own address-bound listener
//! accepted, the host-side restriction the user asked for still holds.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};

use crate::iptables::{self, is_installed, nat_prerouting, run};

/// iptables `--comment` tag prefix; the suffix is the container's full ID.
const RULE_COMMENT_PREFIX: &str = "arcbox-publish:";

/// One published port whose dockerd rule is pinned to a specific host IP.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinnedPublish {
    /// Protocol as dockerd spells it (`tcp`, `udp`, `sctp`).
    pub protocol: String,
    /// Port the host listener is bound to.
    pub host_port: u16,
    /// Container port behind it.
    pub container_port: u16,
    /// The container's address on the network the binding was made on.
    pub container_ip: Ipv4Addr,
}

/// The bindings a container has to a specific IPv4 host address.
///
/// Read from its inspect JSON. Bindings to `0.0.0.0`, the empty address, or
/// an IPv6 address are not returned: dockerd's rules for those already match
/// the relay's traffic (or, for IPv6, cannot be reached by it at all).
#[must_use]
pub fn pinned_publishes(inspect: &serde_json::Value) -> Vec<PinnedPublish> {
    let Some(ports) = inspect
        .pointer("/NetworkSettings/Ports")
        .and_then(|p| p.as_object())
    else {
        return Vec::new();
    };
    let Some(container_ip) = container_ipv4(inspect) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (container_spec, host_bindings) in ports {
        let Some((port, protocol)) = container_spec.split_once('/') else {
            continue;
        };
        let Ok(container_port) = port.parse::<u16>() else {
            continue;
        };
        let Some(bindings) = host_bindings.as_array() else {
            continue;
        };
        for binding in bindings {
            let host_ip = binding
                .get("HostIp")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let Ok(host_ip) = host_ip.parse::<Ipv4Addr>() else {
                continue;
            };
            if host_ip.is_unspecified() {
                continue;
            }
            let Some(host_port) = binding
                .get("HostPort")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|p| *p != 0)
            else {
                continue;
            };
            out.push(PinnedPublish {
                protocol: protocol.to_string(),
                host_port,
                container_port,
                container_ip,
            });
        }
    }
    out.sort_by(|a, b| {
        (a.host_port, &a.protocol, a.container_port).cmp(&(
            b.host_port,
            &b.protocol,
            b.container_port,
        ))
    });
    out.dedup();
    out
}

/// The container's IPv4 address on its first attached network.
fn container_ipv4(inspect: &serde_json::Value) -> Option<Ipv4Addr> {
    inspect
        .pointer("/NetworkSettings/Networks")?
        .as_object()?
        .values()
        .find_map(|net| net.get("IPAddress")?.as_str()?.parse().ok())
}

/// The PREROUTING spec (everything after the chain name) that mirrors one
/// binding for traffic arriving on `uplink`.
fn rule_spec(uplink: &str, container_id: &str, publish: &PinnedPublish) -> Vec<String> {
    vec![
        "-i".into(),
        uplink.into(),
        "-p".into(),
        publish.protocol.clone(),
        "--dport".into(),
        publish.host_port.to_string(),
        "-m".into(),
        "comment".into(),
        "--comment".into(),
        format!("{RULE_COMMENT_PREFIX}{container_id}"),
        "-j".into(),
        "DNAT".into(),
        "--to-destination".into(),
        format!("{}:{}", publish.container_ip, publish.container_port),
    ]
}

/// Installed mirror rules, keyed by container ID.
#[derive(Default)]
pub struct PublishMirror {
    /// Uplink interface the relay's traffic arrives on.
    uplink: String,
    rules: HashMap<String, Vec<Vec<String>>>,
}

impl PublishMirror {
    /// A mirror for traffic arriving on `uplink` (the guest's eth0).
    #[must_use]
    pub fn new(uplink: String) -> Self {
        Self {
            uplink,
            rules: HashMap::new(),
        }
    }

    /// Installs the mirror rules for a container's pinned publishes,
    /// replacing whatever this mirror previously held for it.
    pub async fn apply(&mut self, container_id: &str, publishes: &[PinnedPublish]) -> Result<()> {
        self.remove(container_id).await?;
        if publishes.is_empty() {
            return Ok(());
        }
        let mut installed = Vec::with_capacity(publishes.len());
        for publish in publishes {
            let spec = rule_spec(&self.uplink, container_id, publish);
            if !is_installed(&nat_prerouting("-C", &spec)).await? {
                run(&nat_prerouting("-I", &spec))
                    .await
                    .with_context(|| format!("mirroring publish {publish:?}"))?;
            }
            installed.push(spec);
        }
        tracing::info!(
            container_id,
            count = installed.len(),
            uplink = %self.uplink,
            "mirrored host-address-pinned publishes for the inbound relay"
        );
        self.rules.insert(container_id.to_owned(), installed);
        Ok(())
    }

    /// Removes the mirror rules for a container. Missing rules are fine.
    pub async fn remove(&mut self, container_id: &str) -> Result<()> {
        let Some(specs) = self.rules.remove(container_id) else {
            return Ok(());
        };
        let mut failures = Vec::new();
        for spec in &specs {
            match is_installed(&nat_prerouting("-C", spec)).await {
                Ok(true) => {
                    if let Err(e) = run(&nat_prerouting("-D", spec)).await {
                        failures.push(e.to_string());
                    }
                }
                Ok(false) => {}
                Err(e) => failures.push(e.to_string()),
            }
        }
        if !failures.is_empty() {
            // Keep the record so a later remove retries the kernel side.
            self.rules.insert(container_id.to_owned(), specs);
            bail!(
                "failed to remove {} publish mirror rule(s) for {container_id}: {}",
                failures.len(),
                failures.join("; ")
            );
        }
        Ok(())
    }
}

/// Deletes every mirror rule in the kernel, whoever installed it.
///
/// Run at agent startup: a restarted agent has no record of the previous
/// process's rules, and dockerd re-creates its own on restart, so the
/// event reconciliation that follows reinstalls what is still needed.
pub async fn remove_all_orphans() -> Result<()> {
    let removed = iptables::sweep(RULE_COMMENT_PREFIX).await?;
    if removed > 0 {
        tracing::info!(removed, "swept publish mirror rules from a previous agent");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(ports: serde_json::Value, ip: &str) -> serde_json::Value {
        serde_json::json!({
            "NetworkSettings": {
                "Ports": ports,
                "Networks": { "bridge": { "IPAddress": ip } }
            }
        })
    }

    #[test]
    fn only_specific_ipv4_host_addresses_are_pinned() {
        let doc = inspect(
            serde_json::json!({
                "80/tcp": [
                    {"HostIp": "127.0.0.1", "HostPort": "32768"},
                    {"HostIp": "0.0.0.0", "HostPort": "8080"},
                    {"HostIp": "", "HostPort": "8081"},
                    {"HostIp": "::1", "HostPort": "8082"}
                ],
                "53/udp": [{"HostIp": "192.168.1.5", "HostPort": "5353"}],
                "9000/tcp": null
            }),
            "172.17.0.2",
        );
        let got = pinned_publishes(&doc);
        assert_eq!(
            got,
            vec![
                PinnedPublish {
                    protocol: "udp".into(),
                    host_port: 5353,
                    container_port: 53,
                    container_ip: "172.17.0.2".parse().unwrap(),
                },
                PinnedPublish {
                    protocol: "tcp".into(),
                    host_port: 32768,
                    container_port: 80,
                    container_ip: "172.17.0.2".parse().unwrap(),
                },
            ]
        );
    }

    #[test]
    fn no_ports_or_no_ip_yields_nothing() {
        assert!(pinned_publishes(&serde_json::json!({})).is_empty());
        let no_ip = serde_json::json!({
            "NetworkSettings": {
                "Ports": {"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "1"}]},
                "Networks": {}
            }
        });
        assert!(pinned_publishes(&no_ip).is_empty());
    }

    #[test]
    fn rule_spec_matches_the_uplink_and_names_the_container() {
        let publish = PinnedPublish {
            protocol: "tcp".into(),
            host_port: 32768,
            container_port: 80,
            container_ip: "172.17.0.2".parse().unwrap(),
        };
        let spec = rule_spec("eth0", "abc123", &publish);
        assert_eq!(
            spec.join(" "),
            "-i eth0 -p tcp --dport 32768 -m comment --comment arcbox-publish:abc123 \
             -j DNAT --to-destination 172.17.0.2:80"
        );
        assert_eq!(
            nat_prerouting("-I", &spec)[..6],
            ["-t", "nat", "-w", "2", "-I", "PREROUTING"]
        );
    }
}
