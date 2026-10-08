//! tarpc service implementation.
//!
//! Dispatches each RPC to the corresponding operation module.
//! Input validation (parse, don't validate) happens here at the RPC
//! boundary — mutation functions only accept validated types.

use std::sync::Arc;

use arcbox_constants::dns::Domain;
use arcbox_helper::HelperError;
use arcbox_helper::HelperService;
use arcbox_helper::validate::{BridgeIface, CliName, CliTarget, DnsPort, SocketTarget, Subnet};
use arcbox_local_ca::PublicAuthority;

use super::mutations;
use super::peer_auth::VerifiedPeer;

#[derive(Clone)]
pub(super) struct HelperServer {
    peer: VerifiedPeer,
    resolvers: Arc<mutations::dns::ResolverStore>,
}

impl HelperServer {
    pub(super) fn new(peer: VerifiedPeer, resolvers: Arc<mutations::dns::ResolverStore>) -> Self {
        Self { peer, resolvers }
    }
}

impl HelperService for HelperServer {
    async fn route_add(
        self,
        _: tarpc::context::Context,
        subnet: String,
        iface: String,
    ) -> Result<(), HelperError> {
        let subnet: Subnet = subnet.parse().map_err(HelperError::validation)?;
        let iface: BridgeIface = iface.parse().map_err(HelperError::validation)?;
        mutations::route::add(&subnet, &iface)
    }

    async fn route_remove(
        self,
        _: tarpc::context::Context,
        subnet: String,
    ) -> Result<(), HelperError> {
        let subnet: Subnet = subnet.parse().map_err(HelperError::validation)?;
        mutations::route::remove(&subnet)
    }

    async fn route_remove_if_owned(
        self,
        _: tarpc::context::Context,
        subnet: String,
        iface: String,
    ) -> Result<bool, HelperError> {
        let subnet: Subnet = subnet.parse().map_err(HelperError::validation)?;
        let iface: BridgeIface = iface.parse().map_err(HelperError::validation)?;
        mutations::route::remove_if_owned(&subnet, &iface)
    }

    async fn dns_install(
        self,
        _: tarpc::context::Context,
        domain: String,
        port: u16,
    ) -> Result<(), HelperError> {
        let domain: Domain = domain.parse().map_err(HelperError::validation)?;
        let port = DnsPort::try_from(port).map_err(HelperError::validation)?;
        tracing::debug!(
            peer_uid = self.peer.uid(),
            peer_identifier = self.peer.identifier(),
            resolver_profile = ?self.peer.resolver_profile(),
            domain = %domain,
            "resolver install requested"
        );
        self.resolvers.install(&domain, port)
    }

    async fn dns_uninstall(
        self,
        _: tarpc::context::Context,
        domain: String,
    ) -> Result<(), HelperError> {
        let domain: Domain = domain.parse().map_err(HelperError::validation)?;
        tracing::debug!(
            peer_uid = self.peer.uid(),
            peer_identifier = self.peer.identifier(),
            resolver_profile = ?self.peer.resolver_profile(),
            domain = %domain,
            "resolver uninstall requested"
        );
        self.resolvers.uninstall(&domain)
    }

    async fn dns_status(
        self,
        _: tarpc::context::Context,
        domain: String,
    ) -> Result<bool, HelperError> {
        let domain: Domain = domain.parse().map_err(HelperError::validation)?;
        self.resolvers.status(&domain)
    }

    async fn hosts_alias_install(self, _: tarpc::context::Context) -> Result<(), HelperError> {
        mutations::hosts::install()
    }

    async fn hosts_alias_uninstall(self, _: tarpc::context::Context) -> Result<(), HelperError> {
        mutations::hosts::uninstall()
    }

    async fn hosts_alias_status(self, _: tarpc::context::Context) -> Result<bool, HelperError> {
        mutations::hosts::status()
    }

    async fn socket_link(
        self,
        _: tarpc::context::Context,
        target: String,
    ) -> Result<(), HelperError> {
        let target: SocketTarget = target.parse().map_err(HelperError::validation)?;
        mutations::socket::link(&target)
    }

    async fn socket_unlink(self, _: tarpc::context::Context) -> Result<(), HelperError> {
        mutations::socket::unlink()
    }

    async fn cli_link(
        self,
        _: tarpc::context::Context,
        name: String,
        target: String,
    ) -> Result<(), HelperError> {
        let name: CliName = name.parse().map_err(HelperError::validation)?;
        let target: CliTarget = target.parse().map_err(HelperError::validation)?;
        mutations::cli::link(&name, &target)
    }

    async fn cli_unlink(self, _: tarpc::context::Context, name: String) -> Result<(), HelperError> {
        let name: CliName = name.parse().map_err(HelperError::validation)?;
        mutations::cli::unlink(&name)
    }

    async fn version(self, _: tarpc::context::Context) -> String {
        // Same shape as `arcbox-helper --version` so Desktop/doctor can parse
        // one format from either path.
        format!("arcbox-helper {}", env!("CARGO_PKG_VERSION"))
    }

    async fn ca_install(self, _: tarpc::context::Context, der: Vec<u8>) -> Result<(), HelperError> {
        let ca = PublicAuthority::try_from(der)
            .map_err(|error| HelperError::validation(error.to_string()))?;
        mutations::ca::install(&ca)
    }

    async fn ca_status(
        self,
        _: tarpc::context::Context,
        der: Vec<u8>,
    ) -> Result<bool, HelperError> {
        let ca = PublicAuthority::try_from(der)
            .map_err(|error| HelperError::validation(error.to_string()))?;
        mutations::ca::status(&ca)
    }

    async fn ca_remove(self, _: tarpc::context::Context, der: Vec<u8>) -> Result<(), HelperError> {
        let ca = PublicAuthority::try_from(der)
            .map_err(|error| HelperError::validation(error.to_string()))?;
        mutations::ca::remove(&ca)
    }
}
