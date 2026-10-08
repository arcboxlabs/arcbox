//! Captured guest identity and current authority for host web services.

use std::net::IpAddr;

use arcbox_connect::v1::OpenMachineWebRequest;
use arcbox_constants::{dns::Domain, ports::TLS_SIGNING_PORT};
use arcbox_dns::{
    NamePattern,
    claims::ResourceScope,
    registry::{Lookup, Registry},
};
use arcbox_engine::{
    machine::{MachineInfo, MachineState, MachineWebPorts, machine_hostname},
    vm::VmIncarnation,
};

use super::super::machine_dns::MACHINE_DNS_OWNER_PREFIX;
use super::{Result, Runtime};
use crate::error::CoreError;

/// A host-captured machine name and physical VM run, without cached DNS authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedWebGuest {
    machine: String,
    incarnation: VmIncarnation,
}

/// One guarded machine identity, exact DNS claim, and complete saved web policies.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedMachineWeb {
    /// Physical guest that owns the connection.
    pub guest: CapturedWebGuest,
    /// Canonical name and bridge IPv4 address authorized by the exact claim.
    pub open: OpenMachineWebRequest,
    /// Revision of the exact machine DNS claim.
    pub claim_revision: u64,
    /// Complete saved backend selection.
    pub ports: MachineWebPorts,
}

impl CapturedMachineWeb {
    /// Reports whether a retained session can apply only a policy replacement.
    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        self.guest == other.guest
            && self.open == other.open
            && self.claim_revision == other.claim_revision
    }
}

impl Runtime {
    /// Captures a machine web session under the machine, VM, and DNS registry guards.
    ///
    /// # Errors
    /// Returns an error if the machine, physical run, bridge IPv4 address, or exact claim is unavailable.
    pub fn capture_machine_web(&self, name: &str) -> Result<CapturedMachineWeb> {
        if name == self.default_machine_name() {
            return Err(CoreError::invalid_state(
                "System VM cannot own a machine web session",
            ));
        }
        let guest = self.capture_web_guest(name)?;
        self.machine_manager
            .with_network_incarnation(name, &guest.incarnation, |machine| {
                let registry = self
                    .dns_registry
                    .read()
                    .expect("DNS registry lock poisoned");
                guest.machine_web(self, machine, &registry)
            })?
    }

    /// Connects to the captured agent. Retain a host hold through native connection and session cleanup.
    ///
    /// # Errors
    /// Returns an error if the captured machine stops, changes, or cannot connect.
    pub fn connect_web_guest_agent(
        &self,
        guest: &CapturedWebGuest,
    ) -> Result<arcbox_engine::agent_client::AgentClient> {
        Ok(self
            .machine_manager
            .connect_agent_expected(&guest.machine, &guest.incarnation)?)
    }

    /// Captures a guest before opening its signing or observation connection.
    ///
    /// # Errors
    /// Returns an error if host admission or the physical VM is unavailable.
    pub fn capture_web_guest(&self, machine: &str) -> Result<CapturedWebGuest> {
        self.ensure_host_network_open()?;
        let incarnation = if machine == self.default_machine_name() {
            self.container_network_incarnation()?
        } else {
            self.machine_manager.network_incarnation(machine)?
        };
        Ok(CapturedWebGuest {
            machine: machine.into(),
            incarnation,
        })
    }

    /// Holds the captured machine until host connection and signing work finish.
    ///
    /// # Errors
    /// Returns an error if the captured machine is stopping, unavailable, or replaced.
    pub fn hold_web_guest(
        &self,
        guest: &CapturedWebGuest,
    ) -> Result<arcbox_engine::machine::HostHold> {
        Ok(self
            .machine_manager
            .host_hold_expected(&guest.machine, &guest.incarnation)?)
    }

    /// Opens the captured guest's signing connection without recapturing by name.
    /// Machine callers must retain a host hold until connection and signing work finish.
    ///
    /// # Errors
    /// Returns an error if the captured VM is unavailable or the connection fails.
    pub fn connect_web_guest_signer(&self, guest: &CapturedWebGuest) -> Result<std::os::fd::RawFd> {
        self.connect_vsock_port_expected(&guest.machine, &guest.incarnation, TLS_SIGNING_PORT)
    }

    /// Publishes only while the captured guest and its current DNS claim remain valid.
    /// The callback must not acquire machine, VM, or DNS registry locks.
    ///
    /// # Errors
    /// Returns an error if host admission closes or the captured VM changes.
    pub fn with_web_guest_claim<T>(
        &self,
        guest: &CapturedWebGuest,
        fqdn: &str,
        publish: impl FnOnce(IpAddr, u64) -> T,
    ) -> Result<Option<T>> {
        let Ok(name) = fqdn.parse::<Domain>() else {
            return Ok(None);
        };
        if guest.machine == self.default_machine_name() {
            return self.with_host_network_incarnation(&guest.machine, &guest.incarnation, || {
                let registry = self
                    .dns_registry
                    .read()
                    .expect("DNS registry lock poisoned");
                self.system_web_service_dns_entry(&registry, &name)
                    .map(|(address, revision)| publish(address, revision))
            });
        }
        self.ensure_host_network_open()?;
        Ok(self.machine_manager.with_network_incarnation(
            &guest.machine,
            &guest.incarnation,
            |machine| {
                let registry = self
                    .dns_registry
                    .read()
                    .expect("DNS registry lock poisoned");
                guest
                    .machine_claim(self, machine, &registry, &name)
                    .map(|(address, revision)| publish(address, revision))
            },
        )?)
    }
}

impl CapturedWebGuest {
    fn machine_web(
        &self,
        runtime: &Runtime,
        machine: &MachineInfo,
        registry: &Registry,
    ) -> Result<CapturedMachineWeb> {
        let hostname = machine_hostname(&self.machine)?;
        let name = ResourceScope::Machine
            .name(&hostname, &runtime.config.network.dns_domain)
            .map_err(CoreError::config)?;
        let Some((IpAddr::V4(address), claim_revision)) =
            self.machine_claim(runtime, machine, registry, &name)
        else {
            return Err(CoreError::invalid_state(
                "machine web IPv4 claim is unavailable",
            ));
        };
        Ok(CapturedMachineWeb {
            guest: self.clone(),
            open: OpenMachineWebRequest {
                machine_name: self.machine.clone(),
                hostname: name.to_string(),
                ipv4_address: address.to_string(),
                ..Default::default()
            },
            claim_revision,
            ports: machine.web_ports,
        })
    }

    fn machine_claim(
        &self,
        runtime: &Runtime,
        machine: &MachineInfo,
        registry: &Registry,
        name: &Domain,
    ) -> Option<(IpAddr, u64)> {
        if machine.state != MachineState::Running {
            return None;
        }
        let hostname = machine_hostname(&self.machine).ok()?;
        let canonical = ResourceScope::Machine
            .name(&hostname, &runtime.config.network.dns_domain)
            .ok()?;
        if name != &canonical {
            return None;
        }
        let bridge: IpAddr = machine.bridge_ip_address.as_ref()?.parse().ok()?;
        match registry.resolve(name) {
            Lookup::Matched {
                claim,
                address,
                revision,
                ..
            } if claim.owner() == format!("{MACHINE_DNS_OWNER_PREFIX}{}", self.machine)
                && claim.authority() == ResourceScope::Machine.authority()
                && matches!(claim.pattern(), NamePattern::Exact(exact) if exact == &canonical)
                && address == bridge =>
            {
                Some((address, revision))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_dns::{
        claims::Claim,
        registry::{InstanceAuthority, InstanceTarget},
    };

    #[tokio::test]
    async fn system_authority_rejects_a_machine_claim_and_a_retired_run() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = crate::runtime::NetworkFixture::default();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().into(),
            ..Default::default()
        })
        .unwrap()
        .with_network_fixture(fixture.clone());
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await
            .unwrap();
        runtime
            .register_machine_dns("machine", "192.168.64.8".parse().unwrap())
            .await
            .unwrap();
        let guest = runtime
            .capture_web_guest(runtime.default_machine_name())
            .unwrap();
        let revision = runtime
            .with_web_guest_claim(&guest, "web.arcbox.local", |_, revision| revision)
            .unwrap();
        assert!(revision.is_some());
        assert_eq!(
            runtime
                .with_web_guest_claim(&guest, "machine.machine.arcbox.local", |_, _| ())
                .unwrap(),
            None
        );
        fixture.restart();
        assert!(
            runtime
                .with_web_guest_claim(&guest, "web.arcbox.local", |_, _| panic!(
                    "retired run must not authorize"
                ))
                .is_err()
        );
    }

    #[tokio::test]
    async fn machine_authority_is_exact_current_and_bound_to_one_canonical_record() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().into(),
            ..Default::default()
        })
        .unwrap();
        runtime
            .machine_manager
            .register_mock_machine("first_vm", 7)
            .unwrap();
        let mut machine = runtime.machine_manager.get("first_vm").unwrap();
        machine.bridge_ip_address = Some("192.168.64.8".into());
        let guest = CapturedWebGuest {
            machine: "first_vm".into(),
            incarnation: VmIncarnation::for_test(),
        };
        let name: Domain = runtime
            .machine_dns_name("first_vm")
            .unwrap()
            .parse()
            .unwrap();
        let mut registry =
            Registry::with_builtin_names(&runtime.config.network.dns_domain).unwrap();
        registry.replace_instance_authorities(vec![InstanceAuthority {
            root: runtime.config.network.dns_domain.clone(),
            target: InstanceTarget::Current,
        }]);
        let address: IpAddr = "192.168.64.8".parse().unwrap();
        let claim = Claim::exclusive(
            NamePattern::Exact(name.clone()),
            "machine:first_vm".into(),
            "machine".into(),
        );
        let revision = registry
            .replace(
                "machine:first_vm".into(),
                vec![claim.clone()],
                Some(address),
            )
            .unwrap();
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            Some((address, revision))
        );
        let captured = guest.machine_web(&runtime, &machine, &registry).unwrap();
        assert_eq!(captured.guest, guest);
        assert_eq!(captured.open.machine_name, "first_vm");
        assert_eq!(captured.open.hostname, "first-vm.machine.arcbox.local");
        assert_eq!(captured.open.ipv4_address, "192.168.64.8");
        assert_eq!(captured.claim_revision, revision);
        machine.web_ports.http_port = "8080".parse().unwrap();
        machine.web_ports.https_port = "off".parse().unwrap();
        let policies = guest.machine_web(&runtime, &machine, &registry).unwrap();
        assert!(captured.same_identity(&policies));
        assert_eq!(policies.ports, machine.web_ports);
        assert_ne!(captured.ports, policies.ports);
        // A record fixture without a live VMM must not bypass production capture.
        assert!(runtime.capture_machine_web("first_vm").is_err());
        assert!(
            runtime
                .capture_machine_web(runtime.default_machine_name())
                .is_err()
        );
        for (fqdn, owner, authority) in [
            ("second.machine.arcbox.local", "machine:second", "machine"),
            ("web.arcbox.local", "container", "container"),
            ("arcbox.local", "system:index", "index"),
        ] {
            registry
                .replace(
                    owner.into(),
                    vec![Claim::exclusive(
                        NamePattern::Exact(fqdn.parse().unwrap()),
                        owner.into(),
                        authority.into(),
                    )],
                    Some(address),
                )
                .unwrap();
        }
        for denied in [
            "second.machine.arcbox.local",
            "x.first-vm.machine.arcbox.local",
            "web.arcbox.local",
            "arcbox.local",
        ] {
            assert_eq!(
                guest.machine_claim(&runtime, &machine, &registry, &denied.parse().unwrap()),
                None
            );
        }
        for state in [MachineState::Stopping, MachineState::Stopped] {
            machine.state = state;
            assert_eq!(
                guest.machine_claim(&runtime, &machine, &registry, &name),
                None
            );
            assert!(guest.machine_web(&runtime, &machine, &registry).is_err());
        }
        machine.state = MachineState::Running;
        machine.bridge_ip_address = None;
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        assert!(guest.machine_web(&runtime, &machine, &registry).is_err());
        machine.bridge_ip_address = Some("192.168.64.9".into());
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        assert!(guest.machine_web(&runtime, &machine, &registry).is_err());
        machine.bridge_ip_address = Some(address.to_string());
        for (owner, authority, pattern) in [
            ("machine:other", "machine", NamePattern::Exact(name.clone())),
            (
                "machine:first_vm",
                "container",
                NamePattern::Exact(name.clone()),
            ),
            (
                "machine:first_vm",
                "machine",
                NamePattern::Descendants("machine.arcbox.local".parse().unwrap()),
            ),
        ] {
            registry.remove("machine:first_vm");
            registry
                .replace(
                    owner.into(),
                    vec![Claim::exclusive(pattern, owner.into(), authority.into())],
                    Some(address),
                )
                .unwrap();
            assert_eq!(
                guest.machine_claim(&runtime, &machine, &registry, &name),
                None
            );
            registry.remove(owner);
        }
        registry
            .replace("machine:first_vm".into(), vec![claim], Some(address))
            .unwrap();
        registry.invalidate_instance_authorities();
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        registry.replace_instance_authorities(vec![]);
        let renewed = guest
            .machine_claim(&runtime, &machine, &registry, &name)
            .unwrap()
            .1;
        assert_ne!(renewed, revision);
        assert!(
            !captured.same_identity(&guest.machine_web(&runtime, &machine, &registry).unwrap())
        );
        registry
            .replace(
                "machine:conflict".into(),
                vec![Claim::exclusive(
                    NamePattern::Exact(name.clone()),
                    "machine:conflict".into(),
                    "machine".into(),
                )],
                Some(address),
            )
            .unwrap();
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        registry.remove("machine:conflict");
        registry.remove("machine:first_vm");
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        let ipv6: IpAddr = "fd00::8".parse().unwrap();
        machine.bridge_ip_address = Some(ipv6.to_string());
        registry
            .replace(
                "machine:first_vm".into(),
                vec![Claim::exclusive(
                    NamePattern::Exact(name),
                    "machine:first_vm".into(),
                    "machine".into(),
                )],
                Some(ipv6),
            )
            .unwrap();
        assert!(guest.machine_web(&runtime, &machine, &registry).is_err());
    }
}
