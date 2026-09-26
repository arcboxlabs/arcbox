//! The task that owns the container-domain rules.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::scans::Scans;
use super::{Command, ContainerFacts, RULE_OWNER, http_port, listeners};
use crate::iptables::{self, NatRule, TaggedRules};

/// Whether bridged frames traverse iptables (see the module docs).
const BRIDGE_NF_CALL_IPTABLES: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";

/// The rules in the kernel and the containers they follow.
pub(super) struct Routes {
    rules: TaggedRules,
    containers: HashMap<String, Tracked>,
}

struct Tracked {
    facts: ContainerFacts,
    scans: Scans,
    /// The rules last installed for the container.
    installed: Vec<NatRule>,
}

impl Routes {
    pub(super) fn new() -> Self {
        Self {
            rules: TaggedRules::new(RULE_OWNER),
            containers: HashMap::new(),
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
        loop {
            let due = self.containers.values().filter_map(|c| c.scans.next).min();
            tokio::select! {
                () = cancel.cancelled() => return,
                command = inbox.recv() => match command {
                    Some(Command::Track(facts)) => self.track(facts),
                    Some(Command::Forget(id)) => self.forget(&id).await,
                    None => return,
                },
                () = sleep_until(due) => self.scan_due().await,
            }
        }
    }

    fn track(&mut self, facts: ContainerFacts) {
        let installed = self
            .containers
            .remove(&facts.id)
            .map(|tracked| tracked.installed)
            .unwrap_or_default();
        let tracked = Tracked {
            facts,
            scans: Scans::starting(Instant::now()),
            installed,
        };
        self.containers.insert(tracked.facts.id.clone(), tracked);
    }

    async fn forget(&mut self, container_id: &str) {
        self.containers.remove(container_id);
        if let Err(e) = self.rules.remove(container_id).await {
            tracing::warn!(container_id, error = %format!("{e:#}"), "failed to remove container domain rules");
        }
    }

    /// Rescans every container whose scan is due and follows its choice.
    async fn scan_due(&mut self) {
        let now = Instant::now();
        for tracked in self.containers.values_mut() {
            if tracked.scans.next.is_none_or(|due| due > now) {
                continue;
            }
            tracked.scans.advance(now);
            let id = tracked.facts.id.as_str();
            let listening = match listeners::read(tracked.facts.pid, &tracked.facts.netns).await {
                Ok(listening) => listening,
                Err(e) => {
                    // An exiting container (its die event follows) or an
                    // init process gone to another network namespace.
                    tracing::debug!(container_id = id, error = %e, "cannot read container listeners");
                    continue;
                }
            };
            // A server between two binds lists nothing: keep what it served.
            if listening.is_empty() && tracked.facts.pin.is_none() {
                continue;
            }
            let http_port =
                http_port::choose(tracked.facts.pin, &listening, &tracked.facts.exposed);
            let wanted = tracked.facts.rules(http_port);
            if wanted == tracked.installed {
                continue;
            }
            match self.rules.replace(id, wanted.clone()).await {
                Ok(()) => {
                    tracing::info!(
                        container_id = id,
                        ?http_port,
                        ?listening,
                        "container domain port 80 rerouted"
                    );
                    tracked.installed = wanted;
                }
                Err(e) => {
                    tracing::warn!(container_id = id, error = %format!("{e:#}"), "failed to route container domain port 80");
                }
            }
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
