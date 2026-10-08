//! Leased observation of managed HTTPS access while the host offers CA authorization.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse};
use tokio::sync::watch;
use tokio::time::Instant;

const LEASE: Duration = Duration::from_secs(3);
const MAX_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(super) struct Access(watch::Sender<State>);

#[derive(Default)]
struct State {
    observation: Option<Observation>,
    sequence: u64,
}

struct Observation {
    id: String,
    hosts: BTreeSet<IpAddr>,
    enabled: bool,
    expires: Instant,
    first: ReconcileHttpsAccessResponse,
}

impl Access {
    pub(super) fn reconcile(
        &self,
        request: ReconcileHttpsAccessRequest,
    ) -> Result<ReconcileHttpsAccessResponse> {
        ensure!(
            !request.observation_id.is_empty(),
            "HTTPS observation ID is empty"
        );
        let hosts = request
            .host_addresses
            .iter()
            .map(|address| address.parse().context("invalid HTTPS host address"))
            .collect::<Result<BTreeSet<IpAddr>>>()?;
        let mut response = ReconcileHttpsAccessResponse::default();
        self.0.send_if_modified(|state| {
            let replaced = state
                .observation
                .as_ref()
                .is_none_or(|old| old.id != request.observation_id);
            if replaced {
                state.observation = Some(Observation {
                    id: request.observation_id,
                    hosts: BTreeSet::new(),
                    enabled: false,
                    expires: Instant::now(),
                    first: ReconcileHttpsAccessResponse::default(),
                });
            }
            let observation = state
                .observation
                .as_mut()
                .expect("observation was installed");
            let changed =
                replaced || observation.hosts != hosts || observation.enabled != request.enabled;
            observation.hosts = hosts;
            observation.enabled = request.enabled;
            // Renew without waking every handshake once per host poll.
            observation.expires = Instant::now() + LEASE;
            response = observation.first.clone();
            changed
        });
        Ok(response)
    }

    /// Called after SNI ownership is checked, before looking up a cached leaf.
    pub(super) async fn observe(&self, name: &str, destination: Ipv4Addr, source: IpAddr) {
        let mut updates = self.0.subscribe();
        let mut id = None;
        self.0.send_if_modified(|state| {
            let Some(observation) = state.observation.as_mut().filter(|observation| {
                observation.enabled
                    && observation.expires > Instant::now()
                    && observation.hosts.contains(&source)
            }) else {
                return false;
            };
            id = Some(observation.id.clone());
            if observation.first.sequence != 0 {
                return false;
            }
            state.sequence = state.sequence.saturating_add(1);
            observation.first = ReconcileHttpsAccessResponse {
                hostname: name.to_ascii_lowercase(),
                destination_ip: destination.to_string(),
                sequence: state.sequence,
                ..Default::default()
            };
            true
        });
        let Some(id) = id else { return };
        let deadline = Instant::now() + MAX_WAIT;
        loop {
            let expires = {
                let state = updates.borrow_and_update();
                let Some(observation) = state.observation.as_ref().filter(|observation| {
                    observation.id == id
                        && observation.enabled
                        && observation.hosts.contains(&source)
                }) else {
                    return;
                };
                observation.expires.min(deadline)
            };
            if expires <= Instant::now() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep_until(expires) => {}
                result = updates.changed() => if result.is_err() { return; },
            }
        }
    }
}

#[cfg(test)]
mod tests;
