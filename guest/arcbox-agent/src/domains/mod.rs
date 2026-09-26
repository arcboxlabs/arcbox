//! Container domains served without a port.
//!
//! `http://web.arcbox.local` reaches the container's IP, so it reaches the
//! container only if it serves port 80, which most development servers do
//! not. For each running container whose HTTP port ([`http_port::choose`]:
//! the `dev.arcbox.http-port` label, or what it listens on) is not 80, the
//! agent DNATs port 80 of every IPv4 address the container has to that
//! port, in nat PREROUTING, tagged `arcbox-domain:<container id>`. The rules
//! go when the container dies, and the ones a previous agent left behind are
//! swept when this one starts.
//!
//! A rule matches the destination and nothing else, so it covers both ways
//! traffic reaches a container: routed from the Mac, arriving on the bridge
//! NIC, and switched between containers on one Docker bridge. The switched
//! path reaches iptables only through `br_netfilter`'s
//! `bridge-nf-call-iptables`. dockerd turns that on only when it needs it
//! (`icc=false`, or no userland proxy; ArcBox runs neither), but the System
//! VM kernel builds `br_netfilter` in, and the built-in default is on: a
//! sibling's connection meets the rule, and conntrack un-NATs the reply on
//! the same switched path. The task warns at startup if that ever changes.

mod facts;
mod http_port;
mod listeners;
mod routes;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use self::facts::ContainerFacts;
use self::routes::Routes;

/// iptables `--comment` tag prefix; the suffix is the container's full ID.
const RULE_OWNER: &str = "arcbox-domain:";

/// Handle to the task that owns the container-domain rules.
pub struct DomainRoutes {
    commands: mpsc::UnboundedSender<Command>,
}

enum Command {
    Track(ContainerFacts),
    Forget(String),
}

impl DomainRoutes {
    /// Starts the task. It sweeps a previous agent's rules, then follows
    /// the containers it is told about until `cancel`.
    #[must_use]
    pub fn spawn(cancel: CancellationToken) -> (Self, JoinHandle<()>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let task = tokio::spawn(Routes::new().run(inbox, cancel));
        (Self { commands }, task)
    }

    /// Starts following a running container, or refreshes what it knows.
    pub fn track(&self, facts: ContainerFacts) {
        // The inbox closes only once the task has stopped for shutdown.
        let _ = self.commands.send(Command::Track(facts));
    }

    /// Stops following a container and removes its rules.
    pub fn forget(&self, container_id: &str) {
        let _ = self.commands.send(Command::Forget(container_id.to_owned()));
    }
}
