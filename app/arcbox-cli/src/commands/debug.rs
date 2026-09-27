//! Container debug shell.
//!
//! `abctl debug <container>` opens an interactive shell that shares the target
//! container's namespaces: network, IPC and UTS, plus real membership in its
//! PID namespace (so `ps` and `kill` speak the container's PIDs), with the
//! container root as the working directory. The shell and its tools (every
//! busybox applet, by name) come from the guest agent, so it works even against
//! shell-less images such as `gcr.io/distroless/static`, and never modifies the
//! container image. See the agent's `machine_exec::debug` module for the
//! mechanism and its trade-offs.

use anyhow::Result;
use arcbox_core::vm_lifecycle::DEFAULT_MACHINE_NAME;
use clap::Args;

#[derive(Args)]
pub struct DebugArgs {
    /// Container name or ID to debug
    pub container: String,
    /// Command to run (default: an interactive shell)
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

/// Executes the debug command: an interactive PTY session inside the target
/// container's namespaces, served by the System VM's agent.
pub async fn execute(args: DebugArgs) -> Result<()> {
    let command = if args.command.is_empty() {
        // The agent root's busybox shell — present regardless of the target
        // image, so distroless works too.
        vec!["/bin/sh".to_string()]
    } else {
        args.command
    };
    super::machine::exec_session_interactive(DEFAULT_MACHINE_NAME, &args.container, command).await
}
