//! Captured guest identity and current authority for host web services.

use std::net::IpAddr;

use arcbox_constants::{dns::Domain, ports::TLS_SIGNING_PORT};
use arcbox_dns::{
    NamePattern,
    claims::ResourceScope,
    registry::{Lookup, Registry},
};
use arcbox_engine::{
    machine::{MachineInfo, MachineState, machine_hostname},
    vm::VmIncarnation,
};

use super::super::machine_dns::MACHINE_DNS_OWNER_PREFIX;
use super::{Result, Runtime};

/// A host-captured machine name and physical VM run, without cached DNS authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedWebGuest {
    machine: String,
    incarnation: VmIncarnation,
}

impl Runtime {
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
        }
        machine.state = MachineState::Running;
        machine.bridge_ip_address = None;
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
        machine.bridge_ip_address = Some("192.168.64.9".into());
        assert_eq!(
            guest.machine_claim(&runtime, &machine, &registry, &name),
            None
        );
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
    }
}
