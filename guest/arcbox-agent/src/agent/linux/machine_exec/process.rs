//! Resolving a request into the process to spawn, before anything forks.

use std::path::{Path, PathBuf};

use arcbox_connect::v1::MachineExecRequest;
use arcbox_pty::RunAs;
use tokio::process::Command;

use crate::agent::exec_error::spawn_error;
use crate::rpc::ErrorResponse;

/// Everything needed to spawn a request's process. Stdio and the
/// `pre_exec` steps are left to the mode that runs it.
pub(super) struct ProcessSpec {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    working_dir: Option<PathBuf>,
    /// Credentials to drop to; `None` keeps the agent's (root).
    pub(super) run_as: Option<RunAs>,
}

impl ProcessSpec {
    /// Resolves `req`, or the error frame explaining why it cannot run.
    pub(super) fn resolve(req: &MachineExecRequest) -> Result<Self, ErrorResponse> {
        let Some((program, args)) = req.cmd.split_first() else {
            return Err(ErrorResponse::new(400, "cmd must not be empty"));
        };
        let run_as = if req.user.is_empty() {
            None
        } else {
            Some(
                arcbox_pty::resolve_user(&req.user)
                    .map_err(|e| ErrorResponse::new(400, e.to_string()))?,
            )
        };
        Ok(Self {
            program: PathBuf::from(program),
            args: args.to_vec(),
            env: req
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            working_dir: (!req.working_dir.is_empty()).then(|| PathBuf::from(&req.working_dir)),
            run_as,
        })
    }

    /// A command for this spec: program, arguments, environment and working
    /// directory.
    pub(super) fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }
        // A host disconnect drops the child; it must not keep running
        // detached in the machine.
        cmd.kill_on_drop(true);
        cmd
    }

    /// The error frame for a failed spawn: 404 when the program is missing.
    pub(super) fn spawn_error(&self, error: std::io::Error) -> ErrorResponse {
        let path = self
            .env
            .iter()
            .find_map(|(k, v)| (k == "PATH").then_some(v.as_str()));
        let working_dir = self
            .working_dir
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or_default();
        spawn_error(&self.program.to_string_lossy(), working_dir, path, error)
    }
}
