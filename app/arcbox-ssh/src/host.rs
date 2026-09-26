//! What the SSH server needs from the machines it logs into.

use std::future::Future;

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest};
use arcbox_engine::agent_client::ExecSessionInput;
use tokio::sync::mpsc;

/// Output of a running machine exec session: frames until the one with
/// `done`, or an error.
pub type ExecOutput = mpsc::Receiver<arcbox_engine::Result<MachineExecOutput>>;

/// The machines an SSH login can reach.
///
/// The seam between the protocol handling and the daemon's runtime: the
/// daemon runs sessions through the machine exec path
/// (`AgentClient::machine_exec_session`), tests through local processes.
pub trait MachineHost: Send + Sync + 'static {
    /// Starts `request` in `machine`, fed from `input`. Dropping the returned
    /// receiver ends the session and the process with it.
    fn exec(
        &self,
        machine: &str,
        request: MachineExecRequest,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> impl Future<Output = anyhow::Result<ExecOutput>> + Send;
}
