//! The task that owns the container-domain rules.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Command, ContainerFacts, RULE_OWNER, http_port};
use crate::iptables::{self, NatRule, TaggedRules};

/// Whether bridged frames traverse iptables (see the module docs).
const BRIDGE_NF_CALL_IPTABLES: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";

/// The rules in the kernel and the containers they follow.
pub(super) struct Routes {
    rules: TaggedRules,
    /// The rules last installed per container ID.
    installed: HashMap<String, Vec<NatRule>>,
}

impl Routes {
    pub(super) fn new() -> Self {
        Self {
            rules: TaggedRules::new(RULE_OWNER),
            installed: HashMap::new(),
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
            tokio::select! {
                () = cancel.cancelled() => return,
                command = inbox.recv() => match command {
                    Some(Command::Track(facts)) => self.track(facts).await,
                    Some(Command::Forget(id)) => self.forget(&id).await,
                    None => return,
                },
            }
        }
    }

    /// Makes the container's rules follow its HTTP port.
    async fn track(&mut self, facts: ContainerFacts) {
        let http_port = http_port::choose(facts.pin);
        let wanted = facts.rules(http_port);
        if self.installed.get(&facts.id) == Some(&wanted) {
            return;
        }
        match self.rules.replace(&facts.id, wanted.clone()).await {
            Ok(()) => {
                tracing::info!(container_id = %facts.id, ?http_port, "container domain port 80 rerouted");
                self.installed.insert(facts.id, wanted);
            }
            Err(e) => {
                tracing::warn!(container_id = %facts.id, error = %format!("{e:#}"), "failed to route container domain port 80");
            }
        }
    }

    async fn forget(&mut self, container_id: &str) {
        self.installed.remove(container_id);
        if let Err(e) = self.rules.remove(container_id).await {
            tracing::warn!(container_id, error = %format!("{e:#}"), "failed to remove container domain rules");
        }
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
