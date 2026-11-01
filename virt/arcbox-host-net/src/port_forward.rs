//! The port's `PortForward` over one datapath's inbound listener manager.

use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};
use arcbox_vm_driver::{Error, ForwardProtocol, ForwardRule, PortForward, Result};
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
    /// Refuses a rule whose host port differs from its guest port, `0`
    /// included: the datapath delivers inbound TCP and UDP to the guest on
    /// the port the host listener bound, not on the rule's guest port (its
    /// System VM consumer relies on dockerd's DNAT for the rest), so such a
    /// rule would reach the wrong guest port.
    async fn add(&mut self, rule: ForwardRule) -> Result<u16> {
        if rule.host_port != rule.guest_port {
            return Err(Error::Network(format!(
                "forward {}:{} -> guest :{}: the host datapath forwards only to the same port",
                rule.host_ip, rule.host_port, rule.guest_port
            )));
        }
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
