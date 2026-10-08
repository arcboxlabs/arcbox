//! Validated host configuration for the machine's own network namespace.

use std::net::Ipv4Addr;

use anyhow::{Result, bail};
use arcbox_connect::v1::{MachineWebPorts, WebPortPolicy, web_port_policy::Selection};
use arcbox_constants::dns::Domain;

use super::super::http_port::Pin;
use super::{EndpointFacts, Resource};

impl EndpointFacts {
    /// Stores the admitted identity with both protocols disabled until a replacement arrives.
    pub(crate) fn machine(name: String, hostname: Domain, address: Ipv4Addr) -> Self {
        Self {
            id: format!("machine:{name}"),
            resource: Resource::Machine { name, hostname },
            pid: std::process::id(),
            netns: "/proc/self/ns/net".into(),
            ips: vec![address],
            primary_ip: address,
            pin: Some(Pin::Off),
            https_pin: Some(Pin::Off),
            exposed: Default::default(),
        }
    }

    /// Validates both protocol policies before replacing either stored selection.
    pub(crate) fn replace_machine_ports(&mut self, ports: &MachineWebPorts) -> Result<()> {
        let http = machine_pin(&ports.http_port)?;
        let https = machine_pin(&ports.https_port)?;
        self.pin = http;
        self.https_pin = https;
        Ok(())
    }
}

fn machine_pin(policy: &WebPortPolicy) -> Result<Option<Pin>> {
    match &policy.selection {
        Some(Selection::Auto(_)) => Ok(None),
        Some(Selection::Off(_)) => Ok(Some(Pin::Off)),
        Some(Selection::Port(port @ 1..=65535)) => Ok(Some(Pin::Port(u16::try_from(*port)?))),
        _ => bail!("complete valid machine web policies are required"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_machine_policies_select_each_protocol_without_partial_updates() {
        let mut facts = EndpointFacts::machine(
            "my_vm".into(),
            "my-vm.machine.arcbox.local".parse().unwrap(),
            "192.168.64.8".parse().unwrap(),
        );
        assert_eq!(facts.owner(), "machine:my_vm");
        assert_eq!(facts.pid, std::process::id());
        assert_eq!(facts.netns.to_str(), Some("/proc/self/ns/net"));
        assert_eq!(
            (facts.pin, facts.https_pin),
            (Some(Pin::Off), Some(Pin::Off))
        );
        for (http, https, expected) in [
            (
                Selection::Auto(Box::default()),
                Selection::Off(Box::default()),
                (None, Some(Pin::Off)),
            ),
            (
                Selection::Off(Box::default()),
                Selection::Auto(Box::default()),
                (Some(Pin::Off), None),
            ),
            (
                Selection::Port(8080),
                Selection::Port(8443),
                (Some(Pin::Port(8080)), Some(Pin::Port(8443))),
            ),
        ] {
            let policy = |selection| WebPortPolicy {
                selection: Some(selection),
                ..Default::default()
            };
            let mut ports = MachineWebPorts {
                http_port: policy(http).into(),
                https_port: policy(https).into(),
                ..Default::default()
            };
            facts.replace_machine_ports(&ports).unwrap();
            assert_eq!((facts.pin, facts.https_pin), expected);
            let accepted = facts.clone();
            ports.http_port = policy(Selection::Port(9000)).into();
            ports.https_port = WebPortPolicy::default().into();
            assert!(facts.replace_machine_ports(&ports).is_err());
            assert_eq!(facts, accepted);
        }
    }
}
