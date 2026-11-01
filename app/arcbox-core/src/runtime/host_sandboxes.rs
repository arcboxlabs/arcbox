//! The host sandbox location, as the runtime holds it.
//!
//! The daemon composes [`HostSandboxes`] and hands it over once; the
//! Sandbox API then reaches it through [`Runtime::get_agent`] with
//! [`HOST_SANDBOX_ROUTE`](super::HOST_SANDBOX_ROUTE), exactly as it reaches
//! the System VM's agent for the nested location.

use std::net::IpAddr;
use std::sync::{Arc, Weak};

use arcbox_engine::agent_client::AgentClient;
use arcbox_host_sandbox::{HostSandboxes, SandboxNames};
use async_trait::async_trait;

use super::{HostSandboxCapabilities, Runtime};

impl Runtime {
    /// Installs the composed host location. Returns `false` when one is
    /// installed already.
    pub fn set_host_sandboxes(&self, host: Arc<HostSandboxes>) -> bool {
        self.host_sandboxes.set(host).is_ok()
    }

    /// The host location, once composed.
    #[must_use]
    pub fn host_sandboxes(&self) -> Option<&Arc<HostSandboxes>> {
        self.host_sandboxes.get()
    }

    pub(super) fn connect_host_sandboxes(&self) -> arcbox_engine::Result<AgentClient> {
        self.host_sandboxes
            .get()
            .ok_or_else(|| {
                arcbox_engine::EngineError::invalid_state(
                    "host sandboxes are not available in this daemon",
                )
            })?
            .connect()
    }

    /// Whether sandbox `id` lives at the host location.
    #[must_use]
    pub fn owns_host_sandbox(&self, id: &str) -> bool {
        self.host_sandboxes()
            .is_some_and(|host| host.owns_sandbox(id))
    }

    /// Whether checkpoint `id` lives at the host location.
    #[must_use]
    pub fn owns_host_snapshot(&self, id: &str) -> bool {
        self.host_sandboxes()
            .is_some_and(|host| host.owns_snapshot(id))
    }

    /// What the host location can do now, or `None` when it is not
    /// composed.
    #[must_use]
    pub fn host_sandbox_capabilities(&self) -> Option<HostSandboxCapabilities> {
        let capabilities = self.host_sandboxes()?.capabilities();
        Some(HostSandboxCapabilities {
            backend: capabilities.backend,
            pause_resume: capabilities.pause_resume,
            pause_unavailable: capabilities.pause_unavailable,
            warm_prespawn: capabilities.warm_prespawn,
            port_publishing: capabilities.port_publishing,
            bind_mounts: capabilities.bind_mounts,
        })
    }
}

/// The host location's [`SandboxNames`]: a live host sandbox's name goes
/// into the same sandbox DNS records the nested location's do.
pub struct RuntimeSandboxNames(Weak<Runtime>);

impl RuntimeSandboxNames {
    /// Names published into `runtime`; once it is gone, nothing is.
    #[must_use]
    pub fn new(runtime: &Arc<Runtime>) -> Self {
        Self(Arc::downgrade(runtime))
    }
}

#[async_trait]
impl SandboxNames for RuntimeSandboxNames {
    async fn register(&self, id: &str, address: IpAddr) -> anyhow::Result<()> {
        let Some(runtime) = self.0.upgrade() else {
            anyhow::bail!("the daemon runtime is gone");
        };
        runtime.register_sandbox_dns(id, address).await?;
        Ok(())
    }

    async fn deregister(&self, id: &str) {
        if let Some(runtime) = self.0.upgrade() {
            runtime.deregister_sandbox_dns(id).await;
        }
    }
}
