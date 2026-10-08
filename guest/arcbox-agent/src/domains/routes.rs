//! The task that owns the container-domain rules.

use std::collections::{BTreeSet, HashMap};

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::http_port::HTTPS_PORT;
use super::https::Https;
use super::scans::Scans;
use super::selection::ALT_HTTPS_PORT;
use super::{
    Command, ContainerFacts, HttpPorts, RULE_OWNER, SelectedRoutes, WebServices, http_port,
    listeners, probe,
};
use crate::iptables::{self, NatRule, TaggedRules};

/// Whether bridged frames traverse iptables (see the module docs).
const BRIDGE_NF_CALL_IPTABLES: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";

/// The rules in the kernel and the containers they follow.
pub(super) struct Routes {
    rules: TaggedRules,
    containers: HashMap<String, Tracked>,
    /// The HTTP port behind each address, for the HTTPS proxy.
    ports: HttpPorts,
    /// Whether the HTTPS proxy is up, so port 443 has somewhere to go.
    https: bool,
    services: WebServices,
}

struct Tracked {
    facts: ContainerFacts,
    scans: Scans,
    probes: probe::Cache,
    selected: SelectedRoutes,
    /// The rules last installed for the container.
    installed: Vec<NatRule>,
}

impl Routes {
    pub(super) fn new(services: WebServices) -> Self {
        Self {
            rules: TaggedRules::new(RULE_OWNER),
            containers: HashMap::new(),
            ports: HttpPorts::default(),
            https: false,
            services,
        }
    }

    pub(super) async fn run(
        mut self,
        mut inbox: mpsc::UnboundedReceiver<Command>,
        cancel: CancellationToken,
    ) {
        match iptables::sweep(RULE_OWNER).await {
            Ok(0) => {}
            Ok(removed) => {
                tracing::info!(
                    removed,
                    "swept container domain rules from a previous agent"
                );
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "failed to sweep stale container domain rules");
            }
        }
        warn_unless_bridged_traffic_is_filtered();
        let proxy = match Https::bind().await {
            Ok(https) => {
                self.https = true;
                Some(tokio::spawn(
                    https.serve(self.ports.clone(), cancel.clone()),
                ))
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "container domains serve no HTTPS");
                None
            }
        };
        loop {
            let due = self.containers.values().map(|c| c.scans.next).min();
            tokio::select! {
                () = cancel.cancelled() => break,
                command = inbox.recv() => match command {
                    Some(Command::Track(facts)) => self.track(facts),
                    Some(Command::Forget(id)) => self.forget(&id).await,
                    None => break,
                },
                () = sleep_until(due) => self.scan_due().await,
            }
        }
        if let Some(proxy) = proxy {
            let _ = proxy.await;
        }
    }

    fn track(&mut self, facts: ContainerFacts) {
        let installed = if let Some(known) = self.containers.remove(&facts.id) {
            let gone: Vec<_> = known
                .facts
                .ips()
                .iter()
                .filter(|ip| !facts.ips().contains(ip))
                .copied()
                .collect();
            self.ports.route(&gone, None);
            known.installed
        } else {
            Vec::new()
        };
        let tracked = Tracked {
            facts,
            scans: Scans::starting(Instant::now()),
            probes: probe::Cache::default(),
            selected: SelectedRoutes::default(),
            installed,
        };
        self.containers.insert(tracked.facts.id.clone(), tracked);
    }

    async fn forget(&mut self, container_id: &str) {
        if let Some(tracked) = self.containers.remove(container_id) {
            self.ports.route(tracked.facts.ips(), None);
        }
        if let Err(e) = self.rules.remove(container_id).await {
            tracing::warn!(container_id, error = %format!("{e:#}"), "failed to remove container domain rules");
        }
    }

    /// Rescans every container whose scan is due and follows its choice.
    async fn scan_due(&mut self) {
        let now = Instant::now();
        for tracked in self.containers.values_mut() {
            if tracked.scans.next > now {
                continue;
            }
            tracked.scans.advance(now);
            let id = tracked.facts.id.as_str();
            let snapshot = match listeners::read(tracked.facts.pid, &tracked.facts.netns).await {
                Ok(listening) => listening,
                Err(e) => {
                    // An exiting container (its die event follows) or an
                    // init process gone to another network namespace.
                    tracing::debug!(container_id = id, error = %e, "cannot read container listeners");
                    self.services.remove(id);
                    continue;
                }
            };
            tracked.probes.observe(&snapshot);
            if let Some(port) = tracked.selected.terminated_port()
                && tracked
                    .facts
                    .ips()
                    .iter()
                    .any(|ip| self.ports.get(*ip) != Some(port))
            {
                // The proxy removes its mapping when the upstream connect fails.
                tracked.probes.invalidate();
            }
            let listening = snapshot.ports();
            let mut native = BTreeSet::new();
            if tracked.facts.https_pin.is_none() {
                for port in [HTTPS_PORT, ALT_HTTPS_PORT] {
                    if listening.contains(&port)
                        && tracked
                            .probes
                            .responds(
                                tracked.facts.primary_ip,
                                port,
                                &tracked.facts.hostname,
                                true,
                            )
                            .await
                    {
                        native.insert(port);
                    }
                }
            }
            let mut candidates = listening.difference(&native).copied().collect();
            let http_port = loop {
                let choice =
                    http_port::choose(tracked.facts.pin, &candidates, &tracked.facts.exposed);
                let Some(port) = choice else { break None };
                if tracked.facts.pin.is_some()
                    || tracked
                        .probes
                        .responds(
                            tracked.facts.primary_ip,
                            port,
                            &tracked.facts.hostname,
                            false,
                        )
                        .await
                {
                    break Some(port);
                }
                candidates.remove(&port);
            };
            let mut selected = SelectedRoutes::choose(http_port, tracked.facts.https_pin, &native);
            if !self.https && selected.terminated_port().is_some() {
                selected.https = None;
            }
            let wanted = tracked.facts.rules(selected);
            if wanted != tracked.installed {
                match self.rules.replace(id, wanted.clone()).await {
                    Ok(()) => {
                        tracing::info!(
                            container_id = id,
                            ?selected,
                            ?listening,
                            "container domain rerouted"
                        );
                        tracked.installed = wanted;
                    }
                    Err(e) => {
                        self.services.remove(id);
                        tracing::warn!(container_id = id, error = %format!("{e:#}"), "failed to route container domain");
                        continue;
                    }
                }
            }
            self.ports
                .route(tracked.facts.ips(), selected.terminated_port());
            tracked.selected = selected;
            self.services.publish(&tracked.facts, selected);
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

fn warn_unless_bridged_traffic_is_filtered() {
    match std::fs::read_to_string(BRIDGE_NF_CALL_IPTABLES) {
        Ok(value) if value.trim() == "1" => {}
        Ok(value) => tracing::warn!(
            value = value.trim(),
            "bridge-nf-call-iptables is off: containers reaching a sibling's domain on port 80 bypass its rule"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "br_netfilter is unavailable: containers reaching a sibling's domain on port 80 bypass its rule"
        ),
    }
}
