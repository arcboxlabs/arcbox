//! The port's `PortForward` over the HV machine's userspace NAT datapath.
//! Same shape as `arcbox-vz-driver`'s: both drivers serve the one
//! `InboundListenerManager` the shared `guest_nat` helper builds.
//!
//! A checkpoint that resumes the guest replaces the machine, and with it
//! the datapath the listeners relay into. The forwards outlive that: the
//! handle moves every rule onto the new machine's manager, on the host port
//! it was bound to, so the forwarder its caller took keeps working.

use std::collections::HashMap;
use std::sync::Arc;

use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};
use arcbox_vm_driver::{ForwardProtocol, ForwardRule, PortForward, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

/// The forwards of one VM, shared by the handle and the forwarder it hands
/// out.
#[derive(Default)]
pub(super) struct Forwards {
    /// The current machine's manager; `None` once the VM has exited.
    manager: Option<InboundListenerManager>,
    /// Each added rule, as the caller named it, with the host port bound.
    rules: HashMap<ForwardRule, u16>,
}

pub(super) type SharedForwards = Arc<Mutex<Forwards>>;

impl Forwards {
    pub(super) fn new(manager: InboundListenerManager) -> SharedForwards {
        Arc::new(Mutex::new(Self {
            manager: Some(manager),
            rules: HashMap::new(),
        }))
    }

    /// Moves every forward onto `manager`, the replacement machine's, on
    /// the host port it had. A rule whose port cannot be bound again is
    /// dropped and logged: its host socket is gone either way.
    pub(super) async fn rehome(&mut self, manager: Option<InboundListenerManager>) {
        if let Some(mut old) = self.manager.take() {
            old.stop_all().await;
        }
        self.manager = manager;
        let Some(manager) = self.manager.as_mut() else {
            self.rules.clear();
            return;
        };
        let mut kept = HashMap::new();
        for (rule, port) in self.rules.drain() {
            match manager
                .add_rule(rule.host_ip, port, rule.guest_port, protocol(rule.protocol))
                .await
            {
                Ok(bound) => {
                    kept.insert(rule, bound);
                }
                Err(e) => tracing::warn!(?rule, "forward lost across the checkpoint: {e}"),
            }
        }
        self.rules = kept;
    }

    /// Stops every forward for good.
    pub(super) async fn close(&mut self) {
        self.rehome(None).await;
    }
}

/// Port forwarding into one VM's `UserNat` NIC.
pub(super) struct InboundForward {
    forwards: SharedForwards,
}

impl InboundForward {
    pub(super) const fn new(forwards: SharedForwards) -> Self {
        Self { forwards }
    }
}

const fn protocol(protocol: ForwardProtocol) -> InboundProtocol {
    match protocol {
        ForwardProtocol::Tcp => InboundProtocol::Tcp,
        ForwardProtocol::Udp => InboundProtocol::Udp,
    }
}

#[async_trait]
impl PortForward for InboundForward {
    async fn add(&mut self, rule: ForwardRule) -> Result<u16> {
        let mut forwards = self.forwards.lock().await;
        let manager = forwards
            .manager
            .as_mut()
            .ok_or_else(|| std::io::Error::other("the VM has exited"))?;
        let bound = manager
            .add_rule(
                rule.host_ip,
                rule.host_port,
                rule.guest_port,
                protocol(rule.protocol),
            )
            .await?;
        forwards.rules.insert(rule, bound);
        Ok(bound)
    }

    async fn remove(&mut self, rule: ForwardRule) {
        let mut forwards = self.forwards.lock().await;
        let Some(bound) = forwards.rules.remove(&rule) else {
            return;
        };
        if let Some(manager) = forwards.manager.as_mut() {
            manager
                .remove_rule(rule.host_ip, bound, protocol(rule.protocol))
                .await;
        }
    }

    async fn clear(&mut self) {
        let mut forwards = self.forwards.lock().await;
        forwards.rules.clear();
        if let Some(manager) = forwards.manager.as_mut() {
            manager.stop_all().await;
        }
    }
}
