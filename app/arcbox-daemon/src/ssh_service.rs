//! The SSH server (`ssh <machine>@arcbox`): a loopback listener whose
//! sessions run on the machine exec path.

use std::sync::Arc;

use anyhow::{Context, Result};
use arcbox_connect::v1::MachineExecRequest;
use arcbox_constants::paths::HostLayout;
use arcbox_constants::ports::SSH_HOST_PORT;
use arcbox_core::error::CoreError;
use arcbox_core::{ExecSessionInput, Runtime};
use arcbox_error::CommonError;
use arcbox_ssh::{ExecOutput, MachineHost, SshKeys, SshServer};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// A bound SSH server, not yet serving.
pub struct SshService {
    listener: TcpListener,
    server: SshServer<RuntimeMachines>,
}

impl SshService {
    /// Binds the server the way the Kubernetes proxy binds: an explicitly
    /// requested port (`--ssh-port`, `0` for any) must bind or startup fails,
    /// while the default [`SSH_HOST_PORT`] is best-effort — a port someone
    /// else holds leaves machines without SSH, not the daemon without
    /// containers.
    ///
    /// # Errors
    ///
    /// Returns an error if a requested port cannot be served.
    pub async fn bind_requested(
        requested: Option<u16>,
        layout: &HostLayout,
        runtime: Arc<Runtime>,
    ) -> Result<Option<Self>> {
        let port = requested.unwrap_or(SSH_HOST_PORT);
        match Self::bind(port, layout, runtime).await {
            Ok(service) => Ok(Some(service)),
            Err(error) if requested.is_some() => Err(error.context("Failed to start SSH server")),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "SSH server unavailable");
                Ok(None)
            }
        }
    }

    /// Loads (or first generates) the keys under the data dir's `ssh/` and
    /// binds the loopback listener. Port `0` asks the OS for an unused port.
    async fn bind(port: u16, layout: &HostLayout, runtime: Arc<Runtime>) -> Result<Self> {
        let keys = SshKeys::load_or_generate(&layout.ssh_dir).context("Failed to load SSH keys")?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("SSH server failed to bind 127.0.0.1:{port}"))?;
        let host_port = listener
            .local_addr()
            .context("Failed to read SSH server address")?
            .port();
        info!(host_port, "SSH server bound");
        Ok(Self {
            listener,
            server: SshServer::new(Arc::new(RuntimeMachines(runtime)), keys),
        })
    }

    /// Serves until `shutdown` is cancelled.
    #[must_use]
    pub fn start(self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let stopped = shutdown.cancelled_owned();
            if let Err(error) = self.server.serve(self.listener, stopped).await {
                tracing::error!(%error, "SSH server stopped");
            }
        })
    }
}

/// The daemon's machines, reached through their guest agents.
struct RuntimeMachines(Arc<Runtime>);

impl MachineHost for RuntimeMachines {
    async fn exec(
        &self,
        machine: &str,
        request: MachineExecRequest,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> Result<ExecOutput> {
        let runtime = Arc::clone(&self.0);
        let name = machine.to_owned();
        // Connecting to a guest agent is a blocking hypervisor call.
        let agent = tokio::task::spawn_blocking(move || runtime.get_agent(&name))
            .await
            .context("agent connect task panicked")?
            .map_err(|e| unreachable_machine(machine, e))?;
        Ok(agent.machine_exec_session(request, input).await?)
    }
}

/// What an SSH client is told when its machine cannot take the session.
fn unreachable_machine(machine: &str, error: CoreError) -> anyhow::Error {
    match error {
        CoreError::Common(CommonError::NotFound(_)) => {
            anyhow::anyhow!("no machine named '{machine}' (see `abctl machine ls`)")
        }
        CoreError::Common(CommonError::InvalidState(state)) => {
            anyhow::anyhow!("{state}; start it with `abctl machine start {machine}`")
        }
        other => other.into(),
    }
}
