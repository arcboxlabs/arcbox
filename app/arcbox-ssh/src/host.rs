//! What the SSH server needs from the machines it logs into.

use std::future::Future;

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest};
use arcbox_engine::agent_client::{ExecSessionInput, ExecSessionOutput};
use tokio::sync::mpsc;

/// The machines an SSH login can reach.
///
/// The seam between the protocol handling and the daemon's runtime: the
/// daemon runs sessions through the machine exec path
/// (`AgentClient::machine_exec_session`), tests through scripted ones.
pub trait MachineHost: Send + Sync + 'static {
    /// Output of the sessions this host starts.
    type Output: ExecOutput;

    /// Starts `request` in `machine`, fed from `input`. Dropping the returned
    /// output ends the session and the process with it.
    fn exec(
        &self,
        machine: &str,
        request: MachineExecRequest,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> impl Future<Output = anyhow::Result<Self::Output>> + Send;

    /// Opens a TCP connection to `host:port` as `machine` sees it, carried
    /// like a session: stdin from `input` goes to the peer (an empty one
    /// ends it), the output is the peer's bytes, then an `eof` frame, then
    /// `done` once both directions are closed.
    fn connect_tcp(
        &self,
        machine: &str,
        host: &str,
        port: u16,
        input: mpsc::Receiver<ExecSessionInput>,
    ) -> impl Future<Output = anyhow::Result<Self::Output>> + Send;
}

/// Output of a running machine exec session.
pub trait ExecOutput: Send + 'static {
    /// The next frame: output until the one with `done`, or the error that
    /// ended the session; `None` after either.
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<arcbox_engine::Result<MachineExecOutput>>> + Send;
}

impl ExecOutput for ExecSessionOutput {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<arcbox_engine::Result<MachineExecOutput>>> + Send {
        Self::recv(self)
    }
}
