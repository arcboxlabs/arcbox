//! Publishes complete resolver leases and retires DNS authority before transport cleanup.

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use arcbox_constants::dns::Domain;
use arcbox_dns::{
    NamePattern,
    registry::{InstanceAuthority, InstanceTarget, Registry},
};
use arcbox_helper::{
    ResolverAuthoritySnapshot, ResolverAuthorityState, ResolverLease, ResolverOwner, client::Client,
};
use arcbox_net::NetworkManager;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

pub async fn start(ctx: &DaemonContext, manager: Arc<NetworkManager>, port: u16) -> Result<()> {
    let (ready, started) = oneshot::channel();
    let publisher = Publisher {
        manager: Arc::clone(&manager),
        domain: ctx.dns_domain.clone(),
        setup: Arc::clone(&ctx.setup_state),
        shutdown: ctx.shutdown.clone(),
        socket: arcbox_helper::socket_path(),
        request: ResolverLease {
            profile: ctx.profile.as_str().into(),
            nonce: *uuid::Uuid::new_v4().as_bytes(),
            revision: 0,
            port,
            roots: Vec::new(),
        },
        authorities: Vec::new(),
    };
    let required = ctx.install_dns_resolver;
    ctx.resolver_service
        .start(publisher.run(ready, required, move |stop| {
            crate::mdns::run(Arc::clone(&manager), stop)
        }))
        .await;
    if required {
        started
            .await
            .context("resolver service stopped before setup")??;
    }
    Ok(())
}

struct Publisher {
    manager: Arc<NetworkManager>,
    domain: Domain,
    setup: Arc<arcbox_api::SetupState>,
    shutdown: CancellationToken,
    socket: String,
    request: ResolverLease,
    authorities: Vec<InstanceAuthority>,
}

impl Publisher {
    async fn run<F: Future<Output = std::io::Result<()>>>(
        mut self,
        ready: oneshot::Sender<Result<()>>,
        required: bool,
        mut run_mirror: impl FnMut(CancellationToken) -> F,
    ) -> Result<()> {
        let mut ready = Some(ready);
        loop {
            self.unavailable();
            self.request.revision = 0;
            match self.connection(&mut ready, &mut run_mirror).await {
                Ok(()) => return Ok(()),
                Err(error) if required && ready.is_some() => {
                    let _ = ready.take().unwrap().send(Err(error));
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(%error, "resolver connection failed; retrying with a new connection");
                }
            }
            tokio::select! {
                () = self.shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    async fn connection<F: Future<Output = std::io::Result<()>>>(
        &mut self,
        ready: &mut Option<oneshot::Sender<Result<()>>>,
        run_mirror: impl FnOnce(CancellationToken) -> F,
    ) -> Result<()> {
        let (client, mut dispatch) = tokio::select! {
            () = self.shutdown.cancelled() => return Ok(()),
            connected = Client::connect_owned_to(&self.socket) => connected?,
        };
        let (start, started) = oneshot::channel();
        let stop = CancellationToken::new();
        let shutdown = self.shutdown.clone();
        let mut mirror = Box::pin(async {
            tokio::select! {
                biased;
                () = stop.cancelled() => Ok(()),
                result = started => {
                    result.context("resolver lease was not acquired")?;
                    run_mirror(stop.clone()).await.context("mDNS mirror failed")
                }
            }
        });
        let mut mirror_done = false;
        let mut dispatch_done = false;
        let mut drive = Box::pin(self.drive(&client, ready, start));
        let result = loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break Ok(()),
                result = &mut dispatch => {
                    dispatch_done = true;
                    break result.context("helper dispatcher task failed")
                        .and_then(|result| result.map_err(Into::into))
                        .and_then(|()| Err(anyhow!("helper dispatcher stopped")));
                }
                result = &mut mirror, if !mirror_done => {
                    mirror_done = true;
                    if result.is_err() { break result; }
                }
                result = &mut drive => break result,
            }
        };
        drop(drive);
        self.unavailable();
        self.setup.set_dns_installed(false);
        stop.cancel();
        let mut errors: Vec<String> = result
            .err()
            .map(|error| format!("{error:#}"))
            .into_iter()
            .collect();
        if !mirror_done && let Err(error) = mirror.as_mut().await {
            errors.push(format!("{error:#}"));
        }
        drop(mirror);
        drop(client);
        if !dispatch_done {
            match dispatch.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(error.to_string()),
            }
        }
        ensure!(errors.is_empty(), "resolver cleanup: {}", errors.join("; "));
        Ok(())
    }

    async fn drive(
        &mut self,
        client: &Client,
        ready: &mut Option<oneshot::Sender<Result<()>>>,
        start: oneshot::Sender<()>,
    ) -> Result<()> {
        let mut changes = self.manager.subscribe_dns_registry();
        let mut epoch = None;
        let mut start = Some(start);
        loop {
            let roots = self.desired();
            if self.request.revision == 0 || self.request.roots != roots {
                self.request.revision = self
                    .request
                    .revision
                    .checked_add(1)
                    .context("resolver revision exhausted")?;
                self.request.roots = roots;
                client.resolver_lease(self.request.clone()).await?;
                epoch = None;
            }
            let snapshot = tokio::select! {
                snapshot = client.resolver_watch(epoch) => snapshot?,
                changed = changes.changed(), if epoch.is_some() => { changed?; continue; }
            };
            epoch = Some(snapshot.epoch);
            if !self.apply(snapshot)? {
                continue;
            }
            if let Some(start) = start.take() {
                self.setup.set_dns_installed(true);
                let _ = start.send(());
            }
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(()));
            }
        }
    }

    fn desired(&self) -> Vec<String> {
        let registry = self.manager.dns_registry();
        let registry = registry.read().expect("DNS registry lock poisoned");
        roots(&registry, &self.domain)
    }

    fn apply(&mut self, snapshot: ResolverAuthoritySnapshot) -> Result<bool> {
        let current: Vec<_> = snapshot
            .roots
            .iter()
            .filter(|root| root.owner == ResolverOwner::CurrentConnection)
            .collect();
        ensure!(
            current.len() == self.request.roots.len()
                && current
                    .iter()
                    .zip(&self.request.roots)
                    .all(|(root, expected)| {
                        root.root == *expected
                            && root.state
                                == ResolverAuthorityState::Active {
                                    port: self.request.port,
                                    accepted_revision: self.request.revision,
                                }
                    }),
            "helper snapshot does not confirm the accepted resolver lease"
        );
        let authorities = snapshot
            .roots
            .into_iter()
            .map(|root| {
                Ok(InstanceAuthority {
                    root: root.root.parse().map_err(anyhow::Error::msg)?,
                    target: match (root.owner, root.state) {
                        (_, ResolverAuthorityState::RetainedUnavailable) => {
                            InstanceTarget::Unavailable
                        }
                        (ResolverOwner::CurrentConnection, _) => InstanceTarget::Current,
                        (_, ResolverAuthorityState::Active { port, .. }) => {
                            InstanceTarget::Other { port }
                        }
                    },
                })
            })
            .collect::<Result<_>>()?;
        Ok(self.project(authorities) == self.request.roots)
    }

    fn unavailable(&mut self) {
        let mut roots: BTreeMap<_, _> = self
            .authorities
            .iter()
            .map(|entry| (entry.root.to_string(), entry.root.clone()))
            .collect();
        for name in self.request.roots.iter().chain(self.desired().iter()) {
            roots.insert(
                name.clone(),
                name.parse().expect("resolver roots are validated"),
            );
        }
        self.project(
            roots
                .into_values()
                .map(|root| InstanceAuthority {
                    root,
                    target: InstanceTarget::Unavailable,
                })
                .collect(),
        );
    }

    fn project(&mut self, authorities: Vec<InstanceAuthority>) -> Vec<String> {
        let registry = self.manager.dns_registry();
        let mut registry = registry.write().expect("DNS registry lock poisoned");
        registry.replace_instance_authorities(authorities.clone());
        self.manager.replace_dns_projection(&registry);
        self.authorities = authorities;
        roots(&registry, &self.domain)
    }
}

fn roots(registry: &Registry, configured: &Domain) -> Vec<String> {
    let mut roots = std::collections::BTreeSet::from([configured.to_string()]);
    for owner in registry.owners() {
        for claim in &registry.get(owner).expect("registry owner exists").claims {
            let (NamePattern::Exact(root) | NamePattern::Descendants(root)) = claim.pattern();
            if root.as_str().rsplit_once('.').map(|(_, tld)| tld) == Some("local") {
                roots.insert(root.to_string());
            }
        }
    }
    roots.into_iter().collect()
}
