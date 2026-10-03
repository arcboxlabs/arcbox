//! The host DNS name of a machine.
//!
//! A running machine resolves as `<hostname>.<local domain>` at its bridge
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
//! created before that rule; the table then answers for the one that
//! started last, and the registration says so.

use std::net::IpAddr;

use arcbox_engine::machine::machine_hostname;

use super::Runtime;

/// Owner-key prefix of machine DNS entries; see the module docs.
pub(super) const MACHINE_DNS_OWNER_PREFIX: &str = "machine:";

impl Runtime {
    /// Publishes `machine` at `ip`, replacing any earlier address. A name
    /// that cannot be a hostname (impossible for a created machine, which
    /// `create` already refused) is logged and not published.
    pub async fn register_machine_dns(&self, machine: &str, ip: IpAddr) {
        let hostname = match machine_hostname(machine) {
            Ok(hostname) => hostname,
            Err(e) => {
                tracing::warn!(machine, error = %e, "machine has no publishable hostname");
                return;
            }
        };
        if let Some(other) = self.machine_holding_hostname(machine, &hostname).await {
            tracing::warn!(
                machine,
                hostname,
                other_machine = other,
                "two machines share one hostname; the name now answers for the one that \
                 started last — rename one of them"
            );
        }
        self.register_dns(&Self::machine_dns_owner(machine), &[hostname], ip)
            .await;
    }

    /// The machine other than `machine` whose published hostname is
    /// `hostname`, if any.
    pub(super) async fn machine_holding_hostname(
        &self,
        machine: &str,
        hostname: &str,
    ) -> Option<String> {
        let own = Self::machine_dns_owner(machine);
        self.dns_entries
            .read()
            .await
            .iter()
            .filter(|(owner, _)| **owner != own)
            .find(|(owner, entry)| {
                owner.starts_with(MACHINE_DNS_OWNER_PREFIX)
                    && entry.hostnames.iter().any(|name| name == hostname)
            })
            .and_then(|(owner, _)| owner.strip_prefix(MACHINE_DNS_OWNER_PREFIX))
            .map(str::to_owned)
    }

    /// Withdraws `machine`'s name; a no-op when it was never published.
    pub async fn deregister_machine_dns(&self, machine: &str) {
        self.deregister_dns_by_id(&Self::machine_dns_owner(machine))
            .await;
    }

    /// The machines whose names are currently published.
    pub async fn registered_machine_dns_names(&self) -> Vec<String> {
        self.dns_entries
            .read()
            .await
            .keys()
            .filter_map(|owner| owner.strip_prefix(MACHINE_DNS_OWNER_PREFIX))
            .map(str::to_owned)
            .collect()
    }

    /// The name `machine` is published under, or `None` when the daemon
    /// serves no local domain or the name cannot be a hostname.
    #[must_use]
    pub fn machine_dns_name(&self, machine: &str) -> Option<String> {
        let hostname = machine_hostname(machine).ok()?;
        self.network_manager
            .dns_domain()
            .map(|domain| format!("{hostname}.{domain}"))
    }

    fn machine_dns_owner(machine: &str) -> String {
        format!("{MACHINE_DNS_OWNER_PREFIX}{machine}")
    }
}
