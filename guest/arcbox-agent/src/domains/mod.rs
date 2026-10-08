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
//! with startup backoff and then every five seconds ([`scans`]). Socket
//! identity changes invalidate protocol discovery, including a server
//! replacing its listener on the same port. Closed listeners lose their routes.
//!
//! With the host's Keychain CA signing leaf CSRs, `https://` works the
//! same way: port 443 reaches the application's native HTTPS on 443 or
//! 8443, or a proxy that terminates TLS and relays to a verified HTTP port
//! ([`https`]). `dev.arcbox.https-port` selects a native TLS port or `off`.
//! Native TLS keeps the application's certificate and handshake unchanged.

mod access;
mod certificates;
mod facts;
mod http_port;
mod http_relay;
mod http_tasks;
mod https;
mod index;
mod listeners;
mod probe;
mod routes;
mod scans;
mod selection;
mod services;

use anyhow::Context;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use self::facts::ContainerFacts;
use self::routes::Routes;
pub use self::selection::{HttpBackend, HttpProtocol, HttpsBackend, SelectedRoutes};
pub use self::services::WebServices;

/// iptables `--comment` tag prefix; the suffix is the container's full ID.
const RULE_OWNER: &str = "arcbox-domain:";

/// Handle to the task that owns the container-domain rules.
pub struct DomainRoutes {
    commands: mpsc::UnboundedSender<Command>,
    services: WebServices,
}

/// Commands processed by the container domain rules task.
pub enum Command {
    Update(String, Option<ContainerFacts>, oneshot::Sender<()>),
    Reconcile(Vec<ContainerFacts>, oneshot::Sender<()>),
    Forget(String, oneshot::Sender<anyhow::Result<()>>),
}

impl DomainRoutes {
    /// Starts the task. It sweeps a previous agent's rules, starts the HTTPS
    /// proxy with the host signing channel, then follows the containers
    /// it is told about until `cancel`.
    #[must_use]
    pub fn spawn(
        dns: &crate::dns_server::GuestDnsServer,
        network: arcbox_constants::container_network::ContainerNetwork,
        cancel: CancellationToken,
    ) -> (Self, JoinHandle<()>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let services = WebServices::new(dns.registry());
        let index = index::Publisher::new(
            dns.registry(),
            dns.domain().clone(),
            network.docker_bridge_gateway(),
        );
        let task = tokio::spawn(Routes::new(services.clone()).run(inbox, index, cancel));
        (Self { commands, services }, task)
    }

    /// Returns the installed web routes filtered by the current DNS resolution.
    pub fn services(&self) -> WebServices {
        self.services.clone()
    }

    /// Acknowledges replacement of one container's tracking after any active scan finishes.
    pub async fn update(&self, id: &str, facts: Option<ContainerFacts>) -> anyhow::Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Update(id.to_owned(), facts, reply))
            .map_err(|_| anyhow::anyhow!("container domain rules task stopped"))?;
        response
            .await
            .context("container domain update response was dropped")
    }

    /// Acknowledges replacement of the complete tracked set after any active scan finishes.
    pub async fn reconcile(&self, facts: Vec<ContainerFacts>) -> anyhow::Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Reconcile(facts, reply))
            .map_err(|_| anyhow::anyhow!("container domain rules task stopped"))?;
        response
            .await
            .context("container domain snapshot response was dropped")
    }

    /// Stops following a container and acknowledges removal of every held rule.
    pub async fn forget(&self, container_id: &str) -> anyhow::Result<()> {
        self.services.remove(container_id);
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Forget(container_id.to_owned(), reply))
            .map_err(|_| anyhow::anyhow!("container domain rules task stopped"))?;
        response
            .await
            .context("container domain cleanup response was dropped")?
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, mpsc::UnboundedReceiver<Command>) {
        Self::test_channel_with_services(WebServices::default())
    }

    #[cfg(test)]
    pub(crate) fn test_channel_with_services(
        services: WebServices,
    ) -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        (Self { commands, services }, inbox)
    }
}
