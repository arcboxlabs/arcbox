//! Host listeners for Kubernetes Services of type LoadBalancer.
//!
//! k3s servicelb publishes every LoadBalancer port on the node — its svclb
//! pods bind the port through CNI portmap — and reports the node address as
//! the Service's load-balancer IP, for which kube-proxy installs its own
//! DNAT. The inbound relay dials the guest's uplink (node) address at the
//! host port, so a host listener on the Service port reaches the Service the
//! same way a Docker publish reaches its container, with no guest-side rule
//! of ours.
//!
//! [`Runtime::apply_kubernetes_load_balancers`] brings the listeners in line
//! with one listing of the guest's Services and records how each port fared
//! for `abctl kubernetes status`; the daemon polls it while Kubernetes runs
//! and calls [`Runtime::close_kubernetes_load_balancers`] when the cluster
//! or the VM goes away.

use std::collections::{BTreeMap, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use arcbox_connect::v1::{
    KubernetesHostPort, KubernetesLoadBalancersResponse, kubernetes_host_port::State,
};

use super::Runtime;
use crate::vm_lifecycle::DEFAULT_MACHINE_NAME;

#[cfg(test)]
mod tests;

/// Prefix of every listener owner key this module creates. Container IDs are
/// hex and sandbox keys start with `sandbox:`, so neither can collide.
const OWNER_PREFIX: &str = "k8s:";

/// How long a port whose listener failed to bind waits before another try.
/// Each try logs a warning, so this also paces the log while a conflict lasts.
const BIND_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Whether `key` names a Kubernetes LoadBalancer listener.
pub(super) fn is_owner_key(key: &str) -> bool {
    key.starts_with(OWNER_PREFIX)
}

/// One Service port the host may forward.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PortKey {
    namespace: String,
    name: String,
    port: u16,
    /// As the Service spells it: `TCP`, `UDP` or `SCTP`.
    protocol: String,
}

impl PortKey {
    /// The owner key of this port's listener.
    fn owner(&self) -> String {
        format!(
            "{OWNER_PREFIX}{}/{}:{}/{}",
            self.namespace, self.name, self.port, self.protocol
        )
    }
}

/// What happened to a port on the last reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// servicelb has not published the port on the node yet.
    Pending,
    /// A host listener is bound.
    Forwarded,
    /// Deliberately not bound; the reason does not go away on retry.
    Skipped(String),
    /// The bind failed; retried after [`BIND_RETRY_INTERVAL`].
    Failed { error: String, at: Instant },
}

impl Outcome {
    fn state(&self) -> State {
        match self {
            Self::Pending => State::Pending,
            Self::Forwarded => State::Forwarded,
            Self::Skipped(_) => State::Skipped,
            Self::Failed { .. } => State::Failed,
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Pending => "waiting for servicelb to publish the port on the node",
            Self::Forwarded => "",
            Self::Skipped(reason) => reason,
            Self::Failed { error, .. } => error,
        }
    }
}

/// The last reconcile's outcome for every LoadBalancer port the guest listed.
#[derive(Default)]
pub(super) struct LoadBalancerPorts(BTreeMap<PortKey, Outcome>);

/// A Kubernetes object name as the API server validates it (RFC 1123 label).
/// The guest is the source, so names are checked before they become keys.
fn is_dns_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The ports a listing asks for, each with whether servicelb published it.
fn requested_ports(listing: &KubernetesLoadBalancersResponse) -> BTreeMap<PortKey, bool> {
    let mut ports = BTreeMap::new();
    if !listing.running {
        return ports;
    }
    for lb in &listing.load_balancers {
        if !is_dns_label(&lb.namespace) || !is_dns_label(&lb.name) {
            tracing::debug!(namespace = %lb.namespace, name = %lb.name, "ignoring a LoadBalancer with an invalid name");
            continue;
        }
        let published = !lb.ingress.is_empty();
        for port in &lb.ports {
            let Some(number) = u16::try_from(port.port).ok().filter(|p| *p != 0) else {
                tracing::debug!(namespace = %lb.namespace, name = %lb.name, port = port.port, "ignoring an out-of-range LoadBalancer port");
                continue;
            };
            let key = PortKey {
                namespace: lb.namespace.clone(),
                name: lb.name.clone(),
                port: number,
                protocol: port.protocol.clone(),
            };
            ports.insert(key, published);
        }
    }
    ports
}

/// Why the daemon will not bind `port` on `ip`, when it knows in advance.
///
/// XNU asks for `PRIV_NETINET_RESERVEDPORT` below port 1024 only when the
/// bind names a specific address (`in_pcbbind`, `bsd/netinet/in_pcb.c`): a
/// non-root process binds `0.0.0.0:80` but gets `EACCES` on
/// `127.0.0.1:80` (measured on macOS 26.4 for TCP and UDP alike).
#[cfg(target_os = "macos")]
fn privileged_bind_refusal(ip: Ipv4Addr, port: u16) -> Option<String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    (port < 1024 && !ip.is_unspecified() && !root).then(|| {
        format!(
            "port {port} is below 1024 and macOS lets only root bind it on {ip}; \
             set [docker] expose_ports_to_lan = true to bind every interface instead"
        )
    })
}

/// Elsewhere the kernel's rule differs, so the bind is simply attempted and
/// a refusal is reported as a failure.
#[cfg(not(target_os = "macos"))]
fn privileged_bind_refusal(_ip: Ipv4Addr, _port: u16) -> Option<String> {
    None
}

/// The relay's protocol name for a Service protocol it can carry.
fn relay_protocol(protocol: &str) -> Option<&'static str> {
    match protocol {
        "TCP" => Some("tcp"),
        "UDP" => Some("udp"),
        _ => None,
    }
}

impl Runtime {
    /// Opens and closes host listeners so that exactly the published ports
    /// of `listing` are forwarded, and records each port's outcome.
    ///
    /// Does nothing once the Kubernetes hold is released: a stop or delete
    /// that raced the listing has closed the listeners and must stay closed.
    pub async fn apply_kubernetes_load_balancers(&self, listing: &KubernetesLoadBalancersResponse) {
        let mut ports = self.kubernetes_lb_ports.lock().await;
        if !self.vm_lifecycle.kubernetes_hold() {
            return;
        }
        let requested = requested_ports(listing);

        let wanted: HashSet<String> = requested
            .iter()
            .filter(|(_, published)| **published)
            .map(|(key, _)| key.owner())
            .collect();
        for owner in self.listener_owner_keys(is_owner_key).await {
            if !wanted.contains(&owner) {
                self.stop_port_forwarding_by_id(&owner).await;
            }
        }

        let bind_ip = self.config.docker.default_publish_address();
        let mut next = BTreeMap::new();
        for (key, published) in requested {
            let previous = ports.0.remove(&key);
            let outcome = if published {
                self.forward_port(&key, bind_ip, previous.as_ref()).await
            } else {
                Outcome::Pending
            };
            log_transition(&key, bind_ip, previous.as_ref(), &outcome);
            next.insert(key, outcome);
        }
        for (key, previous) in &ports.0 {
            if previous == &Outcome::Forwarded {
                tracing::info!(namespace = %key.namespace, name = %key.name, port = key.port, protocol = %key.protocol, "Kubernetes LoadBalancer port closed");
            }
        }
        ports.0 = next;
    }

    /// Keeps, retries, or opens the listener of one published port.
    async fn forward_port(
        &self,
        key: &PortKey,
        bind_ip: Ipv4Addr,
        previous: Option<&Outcome>,
    ) -> Outcome {
        let owner = key.owner();
        match previous {
            Some(Outcome::Forwarded) if self.has_port_forwarding(&owner).await => {
                return Outcome::Forwarded;
            }
            Some(skipped @ Outcome::Skipped(_)) => return skipped.clone(),
            Some(failed @ Outcome::Failed { at, .. }) if at.elapsed() < BIND_RETRY_INTERVAL => {
                return failed.clone();
            }
            _ => {}
        }
        let Some(protocol) = relay_protocol(&key.protocol) else {
            return Outcome::Skipped(format!(
                "the host relay forwards TCP and UDP, not {}",
                key.protocol
            ));
        };
        if let Some(reason) = privileged_bind_refusal(bind_ip, key.port) {
            return Outcome::Skipped(reason);
        }
        let binding = (bind_ip.to_string(), key.port, key.port, protocol.to_owned());
        match self
            .start_port_forwarding_for(DEFAULT_MACHINE_NAME, &owner, &[binding])
            .await
        {
            Ok(()) => Outcome::Forwarded,
            Err(e) => Outcome::Failed {
                error: e.to_string(),
                at: Instant::now(),
            },
        }
    }

    /// Closes every Kubernetes LoadBalancer listener and forgets the ports.
    pub async fn close_kubernetes_load_balancers(&self) {
        let mut ports = self.kubernetes_lb_ports.lock().await;
        let owners = self.listener_owner_keys(is_owner_key).await;
        for owner in &owners {
            self.stop_port_forwarding_by_id(owner).await;
        }
        if !owners.is_empty() {
            tracing::info!(
                count = owners.len(),
                "closed Kubernetes LoadBalancer listeners"
            );
        }
        ports.0.clear();
    }

    /// How each LoadBalancer port fared on the last reconcile.
    pub(super) async fn kubernetes_host_ports(&self) -> Vec<KubernetesHostPort> {
        let host_ip = self.config.docker.default_publish_address().to_string();
        let ports = self.kubernetes_lb_ports.lock().await;
        ports
            .0
            .iter()
            .map(|(key, outcome)| KubernetesHostPort {
                namespace: key.namespace.clone(),
                name: key.name.clone(),
                protocol: key.protocol.clone(),
                port: u32::from(key.port),
                host_ip: host_ip.clone(),
                state: outcome.state().into(),
                detail: outcome.detail().to_owned(),
                ..Default::default()
            })
            .collect()
    }
}

/// Logs a port's outcome when it differs from the previous reconcile's.
fn log_transition(key: &PortKey, bind_ip: Ipv4Addr, previous: Option<&Outcome>, now: &Outcome) {
    let unchanged = match (previous, now) {
        (Some(Outcome::Failed { error: was, .. }), Outcome::Failed { error: is, .. }) => was == is,
        (Some(was), is) => was == is,
        (None, _) => false,
    };
    if unchanged {
        return;
    }
    let (namespace, name, port, protocol) = (&key.namespace, &key.name, key.port, &key.protocol);
    if previous == Some(&Outcome::Forwarded) {
        tracing::info!(%namespace, %name, port, %protocol, "Kubernetes LoadBalancer port closed");
    }
    match now {
        Outcome::Pending => {
            tracing::debug!(%namespace, %name, port, %protocol, "Kubernetes LoadBalancer port not published yet");
        }
        Outcome::Forwarded => {
            tracing::info!(%namespace, %name, port, %protocol, host = %bind_ip, "Kubernetes LoadBalancer port forwarded");
        }
        Outcome::Skipped(reason) => {
            tracing::warn!(%namespace, %name, port, %protocol, %reason, "Kubernetes LoadBalancer port not forwarded");
        }
        Outcome::Failed { error, .. } => {
            tracing::warn!(%namespace, %name, port, %protocol, %error, "Kubernetes LoadBalancer port failed to bind; retrying");
        }
    }
}
