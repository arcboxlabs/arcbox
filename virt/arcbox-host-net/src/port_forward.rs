//! The port's `PortForward` over one datapath's inbound listener manager.

use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};
use arcbox_vm_driver::{ForwardProtocol, ForwardRule, PortForward, Result};
use async_trait::async_trait;

/// Port forwarding into one VM through the datapath this network runs for
/// it. Its listeners are host forwarding state: they stay bound after the
/// lease is quarantined until [`PortForward::clear`], which is the
/// host cleanup the quarantine protocol waits for.
pub struct InboundForward {
    manager: InboundListenerManager,
}

impl InboundForward {
    pub fn new(manager: InboundListenerManager) -> Self {
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
    /// The datapath delivers to the rule's guest port whatever the host
    /// listener bound, so the host port may differ from it or be `0`.
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
