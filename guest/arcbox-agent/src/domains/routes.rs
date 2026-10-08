//! The task that owns the container-domain rules.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::http_port::HTTPS_PORT;
use super::https::Https;
use super::index::Publisher;
use super::scans::Scans;
use super::selection::ALT_HTTPS_PORT;
use super::{
    Command, ContainerFacts, RULE_OWNER, SelectedRoutes, WebServices, http_port, listeners, probe,
};
use crate::iptables::{self, NatRule, TaggedRules};

/// Whether bridged frames traverse iptables (see the module docs).
const BRIDGE_NF_CALL_IPTABLES: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";
const INDEX_RETRY: Duration = Duration::from_secs(1);

/// The rules in the kernel and the containers they follow.
pub(super) struct Routes {
    rules: TaggedRules,
    containers: HashMap<String, Tracked>,
    /// Whether managed HTTPS proxy binding has not failed.
    https: bool,
    services: WebServices,
    index: Option<Publisher>,
    index_retry: Option<Instant>,
    frontend: tokio::task::JoinSet<()>,
    frontend_cancel: CancellationToken,
}

impl Drop for Routes {
    fn drop(&mut self) {
        if let Some(index) = &self.index {
            index.withdraw();
        }
        self.frontend_cancel.cancel();
    }
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
            https: true,
            services,
            index: None,
            index_retry: None,
            frontend: tokio::task::JoinSet::new(),
            frontend_cancel: CancellationToken::new(),
        }
    }

    pub(super) async fn run(
        mut self,
        inbox: mpsc::UnboundedReceiver<Command>,
        index: Publisher,
        cancel: CancellationToken,
    ) {
        self.index = Some(index);
        self.cleanup_index().await;
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
        match Https::bind().await {
            Ok(https) => {
                self.frontend
                    .spawn(https.serve(self.services.clone(), self.frontend_cancel.clone()));
                self.reconcile_index().await;
            }
            Err(e) => {
                self.https = false;
                tracing::warn!(error = %format!("{e:#}"), "container domains serve no HTTPS");
            }
        }
        self.follow(inbox, cancel).await;
        self.stop_frontend().await;
        self.cleanup_index().await;
    }

    async fn cleanup_index(&self) {
        if let Some(index) = &self.index
            && let Err(error) = index.cleanup().await
        {
            tracing::warn!(%error, "index redirect cleanup failed");
        }
    }

    async fn follow(
        &mut self,
        mut inbox: mpsc::UnboundedReceiver<Command>,
        cancel: CancellationToken,
    ) {
        loop {
            let due = self.next_scan();
            tokio::select! {
                () = cancel.cancelled() => break,
                Some(result) = self.frontend.join_next() => {
                    if let Err(error) = result { tracing::warn!(%error, "domain frontend exited"); }
                    break;
                },
                command = inbox.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    None => break,
                },
                () = sleep_until(due) => self.scan_due().await,
            }
        }
    }

    async fn stop_frontend(&mut self) {
        if let Some(index) = &self.index {
            index.withdraw();
        }
        self.frontend_cancel.cancel();
        while let Some(result) = self.frontend.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "domain frontend join failed");
            }
        }
    }

    fn next_scan(&self) -> Option<Instant> {
        self.containers
            .values()
            .map(|container| container.scans.next)
            .chain(self.index_retry)
            .min()
    }

    async fn reconcile_index(&mut self) {
        if self.https
            && let Some(index) = &mut self.index
        {
            let result = index.reconcile().await;
            self.index_reconciled(result);
        }
    }

    fn index_reconciled(&mut self, result: anyhow::Result<()>) {
        self.index_retry = None;
        if let Err(error) = result {
            self.index_retry = Some(Instant::now() + INDEX_RETRY);
            tracing::warn!(%error, "index is unavailable");
        }
    }

    async fn handle_command(&mut self, command: Command) {
        // Shutdown can drop the caller while the actor completes a queued command.
        match command {
            Command::Update(id, facts, reply) => {
                match facts {
                    Some(facts) => self.track(facts),
                    None => self.untrack(&id),
                }
                let _ = reply.send(());
            }
            Command::Reconcile(facts, reply) => {
                self.reconcile_index().await;
                let present: BTreeSet<_> = facts.iter().map(|facts| facts.id.clone()).collect();
                self.containers.retain(|id, _| {
                    self.services.remove(id);
                    present.contains(id)
                });
                for facts in facts {
                    self.track(facts);
                }
                let _ = reply.send(());
            }
            Command::Forget(id, reply) => {
                let _ = reply.send(self.forget(&id).await);
            }
        }
    }

    fn track(&mut self, facts: ContainerFacts) {
        self.services.remove(&facts.id);
        let installed = if let Some(known) = self.containers.remove(&facts.id) {
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

    fn untrack(&mut self, container_id: &str) {
        self.containers.remove(container_id);
        self.services.remove(container_id);
    }

    async fn forget(&mut self, container_id: &str) -> anyhow::Result<()> {
        self.untrack(container_id);
        self.rules.remove(container_id).await
    }

    /// Rescans every container whose scan is due and follows its choice.
    async fn scan_due(&mut self) {
        self.reconcile_index().await;
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
            if tracked.selected.terminated_port().is_some() && !self.services.contains(id) {
                // A failed upstream connection removes the installed route.
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
            let selected = SelectedRoutes::choose(http_port, tracked.facts.https_pin, &native)
                .with_managed_https(self.https);
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
            tracked.selected = selected;
            self.services.publish(&tracked.facts, selected);
        }
    }
}

impl SelectedRoutes {
    fn with_managed_https(mut self, enabled: bool) -> Self {
        if !enabled && self.terminated_port().is_some() {
            self.https = None;
        }
        self
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

#[cfg(test)]
mod tests {
    use super::super::HttpsBackend;
    use super::super::http_port::Pin;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn failed_index_retries_without_containers_and_success_clears_the_deadline() {
        use std::{pin::pin, task::Poll};

        let mut routes = Routes::new(WebServices::new(crate::dns_server::DnsRegistry::default()));
        assert!(routes.next_scan().is_none());
        routes.index_reconciled(Err(anyhow::anyhow!("transient rule failure")));
        let mut retry = pin!(sleep_until(routes.next_scan()));
        assert!(matches!(futures::poll!(&mut retry), Poll::Pending));
        tokio::time::advance(INDEX_RETRY.checked_sub(Duration::from_millis(1)).unwrap()).await;
        assert!(matches!(futures::poll!(&mut retry), Poll::Pending));
        tokio::time::advance(Duration::from_millis(1)).await;
        retry.await;
        routes.index_reconciled(Ok(()));
        assert!(routes.next_scan().is_none());
    }

    #[tokio::test]
    async fn actor_exit_withdraws_index_before_cancelling_and_joining_frontend() {
        use arcbox_dns::{NamePattern, claims::Claim};
        for stop in ["inbox", "cancel"] {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let dns = crate::dns_server::DnsRegistry::default();
                let apex: arcbox_constants::dns::Domain = "arcbox.local".parse().unwrap();
                let gateway = "172.17.0.1".parse().unwrap();
                dns.write()
                    .unwrap()
                    .replace(
                        "system:index".into(),
                        vec![Claim::exclusive(
                            NamePattern::Exact(apex.clone()),
                            "system:index".into(),
                            "index".into(),
                        )],
                        Some(std::net::IpAddr::V4(gateway)),
                    )
                    .unwrap();
                let mut routes = Routes::new(WebServices::new(dns.clone()));
                routes.index = Some(Publisher::new(dns.clone(), apex, gateway));
                let child = routes.frontend_cancel.clone();
                let (observed, observation) = tokio::sync::oneshot::channel();
                routes.frontend.spawn(async move {
                    child.cancelled().await;
                    observed
                        .send(dns.read().unwrap().get("system:index").is_none())
                        .unwrap();
                });
                let (sender, inbox) = mpsc::unbounded_channel();
                let cancel = CancellationToken::new();
                let mut sender = Some(sender);
                if stop == "inbox" {
                    drop(sender.take());
                } else {
                    cancel.cancel();
                }
                routes.follow(inbox, cancel).await;
                routes.stop_frontend().await;
                assert!(observation.await.unwrap(), "{stop}");
                assert!(routes.frontend.is_empty());
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn acknowledged_updates_discard_late_scan_publications_and_refresh_tracking() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        for operation in ["reconcile", "update", "remove"] {
            let (domains, mut commands) = super::super::DomainRoutes::test_channel();
            let services = domains.services();
            let mut routes = Routes::new(services.clone());
            let old: Vec<_> = ["live", "absent", "nofacts"]
                .map(|id| {
                    ContainerFacts::from_inspect(
                        &serde_json::json!({
                            "Id": id, "Name": format!("/{id}"), "State": { "Pid": 1 },
                            "NetworkSettings": {
                                "SandboxKey": "/var/run/docker/netns/old",
                                "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
                            }
                        }),
                        &Default::default(),
                    )
                    .unwrap()
                })
                .into();
            let selected = SelectedRoutes::choose(Some(3000), None, &BTreeSet::new());
            for facts in &old {
                routes.track(facts.clone());
            }
            let installed = old[0].rules(selected);
            routes.containers.get_mut("live").unwrap().installed = installed.clone();
            routes.containers.get_mut("live").unwrap().selected = selected;
            let mut changed = old[0].clone();
            changed.pid = 2;
            changed.netns = "/var/run/docker/netns/new".into();
            let request = async {
                match operation {
                    "reconcile" => domains.reconcile(vec![changed.clone()]).await,
                    "update" => domains.update("live", Some(changed.clone())).await,
                    "remove" => domains.update("live", None).await,
                    _ => unreachable!(),
                }
            };
            tokio::pin!(request);
            let command = tokio::select! {
                biased;
                result = &mut request => panic!("request completed before the actor: {result:?}"),
                command = commands.recv() => command.unwrap(),
            };
            // A scan already running can publish after the caller queues its update.
            for facts in &old {
                services.publish(facts, selected);
                assert!(services.contains(&facts.id));
            }
            poll_fn(|context| {
                assert!(request.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            routes.handle_command(command).await;
            request.await.unwrap();

            assert!(!services.contains("live"), "{operation}");
            if operation == "remove" {
                assert!(!routes.containers.contains_key("live"));
            } else {
                let current = &routes.containers["live"];
                assert_eq!(current.facts, changed, "{operation}");
                assert_eq!(current.selected, SelectedRoutes::default(), "{operation}");
                assert_eq!(current.installed, installed, "{operation}");
            }
            for id in ["absent", "nofacts"] {
                let retained = operation != "reconcile";
                assert_eq!(routes.containers.contains_key(id), retained, "{operation}");
                assert_eq!(services.contains(id), retained, "{operation}");
            }
        }
    }

    #[test]
    fn proxy_availability_controls_managed_routes() {
        let routes = Routes::new(WebServices::default());
        for (pin, native, expected) in [
            (None, BTreeSet::new(), Some(HttpsBackend::Terminate(8080))),
            (
                None,
                BTreeSet::from([8443]),
                Some(HttpsBackend::Native(8443)),
            ),
            (
                Some(Pin::Port(9443)),
                BTreeSet::new(),
                Some(HttpsBackend::Native(9443)),
            ),
            (Some(Pin::Off), BTreeSet::from([443]), None),
        ] {
            let selected =
                SelectedRoutes::choose(Some(8080), pin, &native).with_managed_https(routes.https);
            assert_eq!(selected.http, Some(8080));
            assert_eq!(selected.https, expected);
        }
        let unavailable =
            SelectedRoutes::choose(Some(8080), None, &BTreeSet::new()).with_managed_https(false);
        assert_eq!(
            unavailable,
            SelectedRoutes {
                http: Some(8080),
                https: None
            }
        );
    }
}
