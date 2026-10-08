//! The host DNS name of a machine.
//!
//! A running machine resolves as `<hostname>.machine.<local domain>` at its bridge
//! NIC address — the address the Mac reaches directly, unlike the uplink's
//! `10.0.2.x` that every machine shares behind its own NAT. The label is
//! the machine's hostname ([`machine_hostname`]: the name with `_` and `.`
//! turned into `-`), so the guest and the Mac call it the same thing; the
//! System VM is `default`. The entries go through the same ownership table
//! as containers and sandboxes, keyed by machine name under an owner prefix
//! of their own so the Docker host-networking reconciler, which treats
//! every unprefixed owner as a container, never tears a machine down as a
//! vanished container.
//!
//! `MachineManager::create` refuses a name whose hostname another machine
//! already has, so two running machines sharing a label can only be ones
//! created before that rule; their conflicting claims withdraw the shared name.

use std::net::IpAddr;

use arcbox_dns::claims::ResourceScope;
use arcbox_engine::machine::machine_hostname;

use super::Runtime;

/// Owner-key prefix of machine DNS entries; see the module docs.
pub(super) const MACHINE_DNS_OWNER_PREFIX: &str = "machine:";

impl Runtime {
    /// Publishes `machine` at `ip`, replacing any earlier address.
    ///
    /// # Errors
    /// Returns an error if the hostname is invalid or host networking admission is closed.
    pub async fn register_machine_dns(&self, machine: &str, ip: IpAddr) -> super::Result<()> {
        let hostname = machine_hostname(machine)?;
        if let Some(other) = self.machine_holding_hostname(machine, &hostname).await {
            tracing::warn!(
                machine,
                hostname,
                other_machine = other,
                "two machines share one hostname; rename one to resolve the DNS conflict"
            );
        }
        let name = ResourceScope::Machine
            .name(&hostname, &self.config.network.dns_domain)
            .map_err(super::CoreError::config)?;
        self.register_dns(&Self::machine_dns_owner(machine), &[name.to_string()], ip)
            .await
    }

    /// The machine other than `machine` whose published hostname is
    /// `hostname`, if any.
    pub(super) async fn machine_holding_hostname(
        &self,
        machine: &str,
        hostname: &str,
    ) -> Option<String> {
        let hostname = ResourceScope::Machine
            .name(hostname, &self.config.network.dns_domain)
            .ok()?;
        let own = Self::machine_dns_owner(machine);
        let registry = self
            .dns_registry
            .read()
            .expect("DNS registry lock poisoned");
        registry.owners()
            .filter(|owner| *owner != own)
            .find(|owner| {
                owner.starts_with(MACHINE_DNS_OWNER_PREFIX)
                    && registry.get(owner).expect("registry owners are registered").claims.iter().any(|claim| {
                        matches!(claim.pattern(), arcbox_dns::NamePattern::Exact(name) if name == &hostname)
                    })
            })
            .and_then(|owner| owner.strip_prefix(MACHINE_DNS_OWNER_PREFIX))
            .map(str::to_owned)
    }

    /// Withdraws `machine`'s name; a no-op when it was never published.
    pub async fn deregister_machine_dns(&self, machine: &str) {
        self.deregister_dns_by_id(&Self::machine_dns_owner(machine))
            .await;
    }

    /// The machines whose names are currently published.
    pub async fn registered_machine_dns_names(&self) -> Vec<String> {
        self.dns_registry
            .read()
            .expect("DNS registry lock poisoned")
            .owners()
            .filter_map(|owner| owner.strip_prefix(MACHINE_DNS_OWNER_PREFIX))
            .map(str::to_owned)
            .collect()
    }

    /// The canonical machine name, or `None` when the complete DNS name is invalid.
    #[must_use]
    pub fn machine_dns_name(&self, machine: &str) -> Option<String> {
        let hostname = machine_hostname(machine).ok()?;
        ResourceScope::Machine
            .name(&hostname, &self.config.network.dns_domain)
            .ok()
            .map(|name| name.to_string())
    }

    fn machine_dns_owner(machine: &str) -> String {
        format!("{MACHINE_DNS_OWNER_PREFIX}{machine}")
    }
}
