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
//!
//! Servers bind late, so a started container's listeners are read again
//! and again for two minutes ([`scans`]), and the rule follows every change
//! of choice. A read that finds nothing listening (a server between two
//! binds) changes nothing, and the rule stays as chosen once the window
//! closes.
//!
//! When the daemon has put the local CA on the share, `https://` works the
//! same way: port 443 of each container with an HTTP port (unless it
//! listens on 443 itself) is redirected to a proxy in the agent that
//! terminates TLS and relays to that HTTP port ([`https`]).

mod facts;
mod http_port;
mod https;
mod listeners;
mod routes;
mod scans;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use self::facts::ContainerFacts;
use self::routes::Routes;

/// iptables `--comment` tag prefix; the suffix is the container's full ID.
const RULE_OWNER: &str = "arcbox-domain:";

/// The HTTP port behind each container address: the rules task writes it,
/// the HTTPS proxy reads it.
#[derive(Clone, Default)]
struct HttpPorts(Arc<RwLock<HashMap<Ipv4Addr, u16>>>);

impl HttpPorts {
    /// Points each of `ips` at `port`, or forgets them.
    fn route(&self, ips: &[Ipv4Addr], port: Option<u16>) {
        let mut ports = self.0.write().expect("HTTP port table lock poisoned");
        for ip in ips {
            match port {
                Some(port) => ports.insert(*ip, port),
                None => ports.remove(ip),
            };
        }
    }

    fn get(&self, ip: Ipv4Addr) -> Option<u16> {
        let ports = self.0.read().expect("HTTP port table lock poisoned");
        ports.get(&ip).copied()
    }
}

/// Handle to the task that owns the container-domain rules.
pub struct DomainRoutes {
    commands: mpsc::UnboundedSender<Command>,
}

enum Command {
    Track(ContainerFacts),
    Forget(String),
}

impl DomainRoutes {
    /// Starts the task. It sweeps a previous agent's rules, starts the HTTPS
    /// proxy if the local CA is on the share, then follows the containers
    /// it is told about until `cancel`.
    #[must_use]
    pub fn spawn(cancel: CancellationToken) -> (Self, JoinHandle<()>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let task = tokio::spawn(Routes::new().run(inbox, cancel));
        (Self { commands }, task)
    }

    /// Starts following a running container, or restarts its scan window.
    pub fn track(&self, facts: ContainerFacts) {
        // The inbox closes only once the task has stopped for shutdown.
        let _ = self.commands.send(Command::Track(facts));
    }

    /// Stops following a container and removes its rules.
    pub fn forget(&self, container_id: &str) {
        let _ = self.commands.send(Command::Forget(container_id.to_owned()));
    }
}
