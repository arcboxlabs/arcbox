//! Publishes the configured index only after its gateway redirects are verified.

use std::net::Ipv4Addr;

use anyhow::{Result, ensure};
use arcbox_constants::dns::Domain;
use arcbox_dns::{NamePattern, claims::Claim};

use crate::dns_server::DnsRegistry;
use crate::iptables::{self, NatRule, TaggedRules};

const RULE_OWNER: &str = "arcbox-index:";
const OWNER: &str = "system:index";

pub(in crate::domains) struct Publisher {
    dns: DnsRegistry,
    apex: Domain,
    gateway: Ipv4Addr,
    rules: TaggedRules,
}

impl Publisher {
    pub(in crate::domains) fn new(dns: DnsRegistry, apex: Domain, gateway: Ipv4Addr) -> Self {
        Self {
            dns,
            apex,
            gateway,
            rules: TaggedRules::new(RULE_OWNER, iptables::SYSTEM_TOOL),
        }
    }

    fn redirect_rules(&self, frontend_port: u16) -> Vec<NatRule> {
        let address = format!("{}/32", self.gateway);
        let proxy = frontend_port.to_string();
        ["80", "443"]
            .map(|port| {
                NatRule::new(
                    RULE_OWNER,
                    "gateway",
                    &["-d", &address, "-p", "tcp", "-m", "tcp", "--dport", port],
                    &["-j", "REDIRECT", "--to-ports", &proxy],
                )
            })
            .into()
    }

    pub(in crate::domains) async fn reconcile(&mut self, frontend_port: u16) -> Result<()> {
        let wanted = self.redirect_rules(frontend_port);
        match self.rules.list_prerouting().await {
            Ok(listing) if precede_docker(&listing, &wanted) => {}
            result => {
                self.withdraw();
                result?;
                self.rules.sweep().await?;
                self.rules.replace("gateway", wanted.clone()).await?;
                ensure!(
                    precede_docker(&self.rules.list_prerouting().await?, &wanted),
                    "index redirects do not precede Docker"
                );
            }
        }
        self.dns
            .write()
            .expect("DNS registry lock poisoned")
            .replace(
                OWNER.into(),
                vec![Claim::exclusive(
                    NamePattern::Exact(self.apex.clone()),
                    OWNER.into(),
                    "index".into(),
                )],
                Some(self.gateway.into()),
            )?;
        Ok(())
    }

    pub(in crate::domains) fn withdraw(&self) {
        self.dns
            .write()
            .expect("DNS registry lock poisoned")
            .remove(OWNER);
    }

    pub(in crate::domains) async fn cleanup(&self) -> Result<()> {
        self.withdraw();
        self.rules.sweep().await?;
        Ok(())
    }
}

fn precede_docker(listing: &str, rules: &[NatRule]) -> bool {
    let preceding: Vec<Vec<String>> = listing
        .lines()
        .filter_map(|line| line.strip_prefix("-A PREROUTING "))
        .map(|spec| spec.split_whitespace().map(iptables::unquote).collect())
        .take_while(|spec: &Vec<String>| !spec.windows(2).any(|pair| pair == ["-j", "DOCKER"]))
        .collect();
    rules.iter().all(|rule| preceding.contains(&rule.spec))
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.withdraw();
    }
}

#[cfg(test)]
mod kernel_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_redirects_must_precede_docker_and_match_the_exact_gateway() {
        let publisher = Publisher::new(
            DnsRegistry::default(),
            "arcbox.local".parse().unwrap(),
            "172.17.0.1".parse().unwrap(),
        );
        let rules = publisher.redirect_rules(51347);
        assert_eq!(
            rules.iter().map(NatRule::spec).collect::<Vec<_>>(),
            [
                "-d 172.17.0.1/32 -p tcp -m tcp --dport 80 -m comment --comment arcbox-index:gateway -j REDIRECT --to-ports 51347",
                "-d 172.17.0.1/32 -p tcp -m tcp --dport 443 -m comment --comment arcbox-index:gateway -j REDIRECT --to-ports 51347",
            ]
        );
        let entries: Vec<_> = rules
            .iter()
            .map(|rule| format!("-A PREROUTING {}", rule.spec()))
            .collect();
        let docker = "-A PREROUTING -m addrtype --dst-type LOCAL -j DOCKER";
        let ours = entries
            .join("\n")
            .replace("arcbox-index:gateway", "\"arcbox-index:gateway\"");
        assert!(precede_docker(&ours, &rules));
        assert!(precede_docker(&format!("{ours}\n{docker}"), &rules));
        for listing in [
            String::new(),
            format!("{docker}\n{ours}"),
            format!("{}\n{docker}\n{}", entries[0], entries[1]),
            ours.replace("172.17.0.1/32", "10.0.2.2/32"),
            ours.replace("51347", "52468"),
        ] {
            assert!(!precede_docker(&listing, &rules), "{listing}");
        }
    }
}
