//! tarpc service implementation.
//!
//! Dispatches each RPC to the corresponding operation module
//! (route, dns, socket). All input validation happens inside
//! those modules before any privileged syscall.

use arcbox_helper::HelperService;

use super::mutations;

#[derive(Clone)]
pub struct HelperServer;

impl HelperService for HelperServer {
    async fn route_add(
        self,
        _: tarpc::context::Context,
        subnet: String,
        iface: String,
    ) -> Result<(), String> {
        mutations::route::add(&subnet, &iface)
    }

    async fn route_remove(self, _: tarpc::context::Context, subnet: String) -> Result<(), String> {
        mutations::route::remove(&subnet)
    }

    async fn dns_install(
        self,
        _: tarpc::context::Context,
        domain: String,
        port: u16,
    ) -> Result<(), String> {
        mutations::dns::install(&domain, port)
    }

    async fn dns_uninstall(self, _: tarpc::context::Context, domain: String) -> Result<(), String> {
        mutations::dns::uninstall(&domain)
    }

    async fn dns_status(self, _: tarpc::context::Context, domain: String) -> Result<bool, String> {
        mutations::dns::status(&domain)
    }

    async fn socket_link(self, _: tarpc::context::Context, target: String) -> Result<(), String> {
        mutations::socket::link(&target)
    }

    async fn socket_unlink(self, _: tarpc::context::Context) -> Result<(), String> {
        mutations::socket::unlink()
    }

    async fn version(self, _: tarpc::context::Context) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}
