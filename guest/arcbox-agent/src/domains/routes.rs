//! The task that owns the container-domain rules.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::certificates::RootSource;
use super::http_port::HTTPS_PORT;
use super::https::Https;
use super::index::Publisher;
use super::scans::Scans;
use super::selection::{ALT_HTTPS_PORT, HttpBackend};
use super::{
    Command, EndpointFacts, RULE_OWNER, SelectedRoutes, WebServices, http_port, listeners, probe,
};
use crate::iptables::{NatRule, TaggedRules};

/// Whether bridged frames traverse iptables (see the module docs).
const BRIDGE_NF_CALL_IPTABLES: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";
const INDEX_RETRY: Duration = Duration::from_secs(1);

/// The rules in the kernel and the containers they follow.
pub(super) struct Routes {
    rules: TaggedRules,
    containers: HashMap<String, Tracked>,
    /// The frontend's bound port; absent when managed HTTPS is unavailable.
    frontend_port: Option<u16>,
    services: WebServices,
    index: Option<Publisher>,
    index_retry: Option<Instant>,
    frontend: tokio::task::JoinSet<()>,
    frontend_cancel: CancellationToken,
}

impl Drop for Routes {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct Tracked {
    facts: EndpointFacts,
    scans: Scans,
    probes: probe::Cache,
    selected: SelectedRoutes,
    /// The rules last installed for the container.
    installed: Vec<NatRule>,
}

impl Routes {
    pub(super) fn new(services: WebServices, executable: impl Into<PathBuf>) -> Self {
        Self {
            rules: TaggedRules::new(RULE_OWNER, executable),
            containers: HashMap::new(),
            frontend_port: None,
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
    ) -> anyhow::Result<()> {
        self.index = Some(index);
        if let Err(error) = Self::cleanup_index(self.index.as_ref()).await {
            tracing::warn!(%error, "index redirect cleanup failed");
        }
        match self.rules.sweep().await {
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
        match Https::bind(RootSource::system()).await {
            Ok((https, port)) => {
                self.frontend_port = Some(port);
                self.frontend
                    .spawn(https.serve(self.services.clone(), self.frontend_cancel.clone()));
                self.reconcile_index().await;
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "container domains serve no HTTPS");
            }
        }
        let result = self.follow(inbox, cancel).await;
        self.finish(result).await
    }

    pub(super) async fn run_machine(
        mut self,
        inbox: mpsc::UnboundedReceiver<Command>,
        root: RootSource,
        cancel: CancellationToken,
        ready: oneshot::Sender<()>,
    ) -> anyhow::Result<()> {
        let result = async {
            self.rules
                .sweep()
                .await
                .context("sweep machine web redirects")?;
            let (https, port) = Https::bind(root).await?;
            self.frontend_port = Some(port);
            self.frontend
                .spawn(https.serve(self.services.clone(), self.frontend_cancel.clone()));
            ready
                .send(())
                .map_err(|()| anyhow::anyhow!("machine web connection ended before readiness"))?;
            self.follow(inbox, cancel.clone()).await
        }
        .await;
        cancel.cancel();
        self.finish(result).await
    }

    async fn finish(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let cleanup = self.shutdown().await;
        match (result, cleanup) {
            (Ok(()), cleanup) => cleanup,
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup)) => Err(error.context(format!("{cleanup:#}"))),
        }
    }

    async fn cleanup_index(index: Option<&Publisher>) -> anyhow::Result<()> {
        if let Some(index) = index {
            index.cleanup().await?;
        }
        Ok(())
    }

    async fn follow(
        &mut self,
        mut inbox: mpsc::UnboundedReceiver<Command>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        loop {
            let due = self.next_scan();
            tokio::select! {
                () = cancel.cancelled() => break,
                Some(result) = self.frontend.join_next() => {
                    result.context("domain frontend task failed")?;
                    anyhow::bail!("domain frontend stopped before shutdown");
                },
                command = inbox.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    None => break,
                },
                () = sleep_until(due) => self.scan_due().await,
            }
        }
        Ok(())
    }

    fn begin_shutdown(&self) {
        self.services.clear();
        if let Some(index) = &self.index {
            index.withdraw();
        }
        self.frontend_cancel.cancel();
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.begin_shutdown();
        finish_shutdown(
            &mut self.frontend,
            self.rules.sweep(),
            Self::cleanup_index(self.index.as_ref()),
        )
        .await
    }

    fn next_scan(&self) -> Option<Instant> {
        self.containers
            .values()
            .map(|container| container.scans.next)
            .chain(self.index_retry)
            .min()
    }

    async fn reconcile_index(&mut self) {
        if let Some(port) = self.frontend_port
            && let Some(index) = &mut self.index
        {
            let result = index.reconcile(port).await;
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

    fn track(&mut self, facts: EndpointFacts) {
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
                            .responds_https(
                                tracked.facts.primary_ip,
                                port,
                                tracked.facts.hostname(),
                            )
                            .await
                    {
                        native.insert(port);
                    }
                }
            }
            let candidates = listening.difference(&native).copied().collect();
            let http = tracked.http_backend(candidates).await;
            let id = tracked.facts.id.as_str();
            let selected = SelectedRoutes::choose(http, tracked.facts.https_pin, &native)
                .with_managed_https(self.frontend_port.is_some());
            let wanted = tracked.facts.rules(selected, self.frontend_port);
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

async fn finish_shutdown(
    frontend: &mut tokio::task::JoinSet<()>,
    domain_cleanup: impl Future<Output = anyhow::Result<usize>>,
    index_cleanup: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    while let Some(result) = frontend.join_next().await {
        if let Err(error) = result {
            failures.push(format!("frontend join: {error}"));
        }
    }
    // Keep redirects until the frontend releases every connection and its listener.
    let (domains, index) = tokio::join!(domain_cleanup, index_cleanup);
    for (owner, result) in [
        ("domain redirects", domains.map(|_| ())),
        ("index redirects", index),
    ] {
        if let Err(error) = result {
            failures.push(format!("{owner}: {error:#}"));
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "domain shutdown failed: {}",
        failures.join("; ")
    );
    Ok(())
}

impl Tracked {
    async fn http_backend(&mut self, mut candidates: BTreeSet<u16>) -> Option<HttpBackend> {
        loop {
            let port = http_port::choose(self.facts.pin, &candidates, &self.facts.exposed)?;
            let protocol = self
                .probes
                .http(self.facts.primary_ip, port, self.facts.hostname())
                .await;
            if self.facts.pin.is_some() || protocol.is_some() {
                return Some(HttpBackend { port, protocol });
            }
            candidates.remove(&port);
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
#[cfg(target_os = "linux")]
mod kernel_tests;

#[cfg(all(test, target_os = "linux"))]
mod machine_tests;

#[cfg(test)]
mod tests {
    use super::super::http_port::Pin;
    use super::super::{HttpProtocol, HttpsBackend};
    use super::*;

    #[tokio::test]
    async fn pinned_unverified_endpoints_remain_selected_while_automatic_candidates_do_not() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                listener
                    .accept()
                    .await
                    .unwrap()
                    .0
                    .write_all(b"-ERR not HTTP\r\n")
                    .await
                    .unwrap();
            }
        });
        let facts = EndpointFacts::from_inspect(
            &serde_json::json!({
                "Id": "pinned", "Name": "/web", "State": { "Pid": 1 },
                "Config": { "Labels": { "dev.arcbox.http-port": port.to_string() } },
                "NetworkSettings": {
                    "SandboxKey": "/var/run/docker/netns/web",
                    "Networks": { "bridge": { "IPAddress": "127.0.0.1" } }
                }
            }),
            &Default::default(),
        )
        .unwrap();
        let mut routes = Routes::new(WebServices::default(), crate::iptables::SYSTEM_TOOL);
        routes.track(facts);
        let tracked = routes.containers.get_mut("pinned").unwrap();
        let backend = tracked.http_backend(BTreeSet::new()).await;
        assert_eq!(
            backend,
            Some(HttpBackend {
                port,
                protocol: None
            })
        );
        let selected = SelectedRoutes::choose(backend, None, &BTreeSet::new());
        assert_eq!(selected.http, Some(port));
        assert_eq!(selected.terminated_backend(), backend);
        tracked.facts.pin = None;
        assert_eq!(tracked.http_backend(BTreeSet::from([port])).await, None);
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_index_retries_without_containers_and_success_clears_the_deadline() {
        use std::{pin::pin, task::Poll};

        let mut routes = Routes::new(
            WebServices::new(crate::dns_server::DnsRegistry::default()),
            crate::iptables::SYSTEM_TOOL,
        );
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
    async fn actor_exit_withdraws_all_publication_before_frontend_teardown() {
        use arcbox_dns::{
            NamePattern,
            claims::Claim,
            registry::{InstanceAuthority, InstanceTarget},
        };
        for stop in ["inbox", "cancel", "drop"] {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let dns = crate::dns_server::DnsRegistry::default();
                let apex: arcbox_constants::dns::Domain = "arcbox.local".parse().unwrap();
                let gateway = "172.17.0.1".parse().unwrap();
                dns.write()
                    .unwrap()
                    .replace_instance_authorities(vec![InstanceAuthority {
                        root: apex.clone(),
                        target: InstanceTarget::Current,
                    }]);
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
                let services = WebServices::new(dns.clone());
                let source = services.directory_source();
                source.ready();
                let name = "web.arcbox.local";
                let ip = "172.17.0.2".parse().unwrap();
                dns.write()
                    .unwrap()
                    .replace(
                        "web".into(),
                        vec![Claim::exclusive(
                            NamePattern::Exact(name.parse().unwrap()),
                            "web".into(),
                            "container".into(),
                        )],
                        Some(std::net::IpAddr::V4(ip)),
                    )
                    .unwrap();
                let facts = EndpointFacts::from_inspect(
                    &serde_json::json!({
                        "Id": "web", "Name": "/web", "State": { "Pid": 1 },
                        "NetworkSettings": {
                            "SandboxKey": "/var/run/docker/netns/web",
                            "Networks": { "bridge": { "IPAddress": ip.to_string() } }
                        }
                    }),
                    &apex,
                )
                .unwrap();
                services.publish(
                    &facts,
                    SelectedRoutes::choose(
                        Some(HttpBackend {
                            port: 3000,
                            protocol: Some(HttpProtocol::Http1),
                        }),
                        None,
                        &BTreeSet::new(),
                    ),
                );
                let pinned = services
                    .observe_managed_tls(name, ip, ip.into())
                    .await
                    .unwrap();
                assert_eq!(services.snapshot(None).len(), 2);
                let mut routes = Routes::new(services.clone(), crate::iptables::SYSTEM_TOOL);
                routes.index = Some(Publisher::new(dns.clone(), apex, gateway));
                let child = routes.frontend_cancel.clone();
                if stop == "drop" {
                    drop(routes);
                    assert!(child.is_cancelled());
                    let snapshot = services.snapshot(None);
                    assert!(snapshot.is_empty(), "{snapshot:?}");
                    assert!(services.verify_tls_route(name, ip, &pinned).is_err());
                    assert!(dns.read().unwrap().get("web").is_some());
                    return;
                }
                let (observed, observation) = tokio::sync::oneshot::channel();
                let frontend_services = services.clone();
                routes.frontend.spawn(async move {
                    child.cancelled().await;
                    observed
                        .send(
                            dns.read().unwrap().get("system:index").is_none()
                                && dns.read().unwrap().get("web").is_some()
                                && frontend_services
                                    .verify_tls_route(name, ip, &pinned)
                                    .is_err(),
                        )
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
                routes.follow(inbox, cancel).await.unwrap();
                routes.begin_shutdown();
                let snapshot = services.snapshot(None);
                assert!(snapshot.is_empty(), "{snapshot:?}");
                finish_shutdown(&mut routes.frontend, async { Ok(0) }, async { Ok(()) })
                    .await
                    .unwrap();
                assert!(observation.await.unwrap(), "{stop}");
                assert!(routes.frontend.is_empty());
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn shutdown_joins_frontend_then_awaits_all_cleanup_and_reports_every_failure() {
        tokio::time::timeout(Duration::from_secs(2), async {
            use std::{cell::Cell, pin::pin};
            use tokio::sync::oneshot;

            let mut routes = Routes::new(WebServices::default(), crate::iptables::SYSTEM_TOOL);
            routes.frontend.spawn(async { panic!("frontend panic") });
            let (release_frontend, frontend_gate) = oneshot::channel();
            let (frontend_done, joined) = oneshot::channel();
            let child = routes.frontend_cancel.clone();
            routes.frontend.spawn(async move {
                child.cancelled().await;
                frontend_gate.await.unwrap();
                frontend_done.send(()).unwrap();
            });
            let (release_domains, domains_gate) = oneshot::channel::<anyhow::Result<usize>>();
            let (release_index, index_gate) = oneshot::channel::<anyhow::Result<()>>();
            let domains_started = Cell::new(false);
            let index_started = Cell::new(false);
            routes.begin_shutdown();
            let error = {
                let mut shutdown = pin!(finish_shutdown(
                    &mut routes.frontend,
                    async {
                        domains_started.set(true);
                        domains_gate.await.unwrap()
                    },
                    async {
                        index_started.set(true);
                        index_gate.await.unwrap()
                    },
                ));
                assert!(futures::poll!(&mut shutdown).is_pending());
                assert!(!domains_started.get() && !index_started.get());
                release_frontend.send(()).unwrap();
                joined.await.unwrap();
                assert!(futures::poll!(&mut shutdown).is_pending());
                assert!(domains_started.get() && index_started.get());
                release_domains
                    .send(Err(anyhow::anyhow!("domain rule busy")))
                    .unwrap();
                assert!(futures::poll!(&mut shutdown).is_pending());
                release_index
                    .send(Err(anyhow::anyhow!("index rule busy")))
                    .unwrap();
                shutdown.await.unwrap_err()
            };
            assert!(routes.frontend.is_empty());
            let message = format!("{error:#}");
            for failure in ["frontend panic", "domain rule busy", "index rule busy"] {
                assert!(message.contains(failure), "{message}");
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn acknowledged_updates_discard_late_scan_publications_and_refresh_tracking() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        for operation in ["reconcile", "update", "remove"] {
            let (domains, mut commands) = super::super::DomainRoutes::test_channel();
            let services = domains.services();
            let mut routes = Routes::new(services.clone(), crate::iptables::SYSTEM_TOOL);
            let old: Vec<_> = ["live", "absent", "nofacts"]
                .map(|id| {
                    EndpointFacts::from_inspect(
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
            let selected = SelectedRoutes::choose(
                Some(HttpBackend {
                    port: 3000,
                    protocol: Some(HttpProtocol::Http1),
                }),
                None,
                &BTreeSet::new(),
            );
            for facts in &old {
                routes.track(facts.clone());
            }
            let installed = old[0].rules(selected, Some(51347));
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
        let mut routes = Routes::new(WebServices::default(), crate::iptables::SYSTEM_TOOL);
        assert!(routes.frontend_port.is_none());
        routes.frontend_port = Some(51347);
        for (pin, native, expected) in [
            (
                None,
                BTreeSet::new(),
                Some(HttpsBackend::Terminate(HttpBackend {
                    port: 8080,
                    protocol: Some(HttpProtocol::Http1),
                })),
            ),
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
            let selected = SelectedRoutes::choose(
                Some(HttpBackend {
                    port: 8080,
                    protocol: Some(HttpProtocol::Http1),
                }),
                pin,
                &native,
            )
            .with_managed_https(routes.frontend_port.is_some());
            assert_eq!(selected.http, Some(8080));
            assert_eq!(selected.https, expected);
        }
        let unavailable = SelectedRoutes::choose(
            Some(HttpBackend {
                port: 8080,
                protocol: Some(HttpProtocol::Http1),
            }),
            None,
            &BTreeSet::new(),
        )
        .with_managed_https(false);
        assert_eq!(
            unavailable,
            SelectedRoutes {
                http: Some(8080),
                https: None
            }
        );
    }
}
