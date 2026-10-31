//! The port's `PortForward` over the HV machine's userspace NAT datapath.
//! Same shape as `arcbox-vz-driver`'s: both drivers serve the one
//! `InboundListenerManager` the shared `guest_nat` helper builds.

use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};
use arcbox_vm_driver::{ForwardProtocol, ForwardRule, PortForward, Result};
use async_trait::async_trait;

/// Port forwarding into one VM's `UserNat` NIC.
pub(super) struct InboundForward {
    manager: InboundListenerManager,
}

impl InboundForward {
    pub(super) fn new(manager: InboundListenerManager) -> Self {
        Self { manager }
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
        Ok(self
            .manager
            .add_rule(
                rule.host_ip,
                rule.host_port,
                rule.guest_port,
                protocol(rule.protocol),
            )
            .await?)
    }

    async fn remove(&mut self, rule: ForwardRule) {
        self.manager
            .remove_rule(rule.host_ip, rule.host_port, protocol(rule.protocol))
            .await;
    }

    async fn clear(&mut self) {
        self.manager.stop_all().await;
    }
}
