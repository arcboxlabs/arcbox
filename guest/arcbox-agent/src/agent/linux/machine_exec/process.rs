//! Resolving a request into the process to spawn — a plain command, or an
//! sshd-style login session — before anything forks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use arcbox_connect::v1::MachineExecRequest;
use arcbox_pty::RunAs;
use tokio::process::Command;

use super::login_path;
use crate::agent::exec_error::spawn_error;
use crate::agent::login_session::{Account, LoginProcess};
use crate::rpc::ErrorResponse;

/// The user a login session runs as when the request names none.
const DEFAULT_LOGIN_USER: &str = "root";

/// Everything needed to spawn a request's process. Stdio and the
/// `pre_exec` steps are left to the mode that runs it.
pub(super) struct ProcessSpec {
    program: PathBuf,
    arg0: Option<OsString>,
    args: Vec<String>,
    /// Start from an empty environment (login sessions) rather than the
    /// agent's own.
    clear_env: bool,
    env: Vec<(String, String)>,
    working_dir: Option<PathBuf>,
    /// Credentials to drop to; `None` keeps the agent's (root).
    pub(super) run_as: Option<RunAs>,
}

impl ProcessSpec {
    /// Resolves `req`, or the error frame explaining why it cannot run.
    pub(super) async fn resolve(req: &MachineExecRequest) -> Result<Self, ErrorResponse> {
        if req.login {
            Self::login(req).await
        } else {
            Self::plain(req).await
        }
    }

    /// A plain command: the agent's environment with the request's applied
    /// over it and, unless the request sets its own, the machine's login
    /// `PATH` in front of the agent's — so an exec finds what a login finds.
    /// A container-debug exec keeps the agent's `PATH`: it runs in the
    /// container's namespaces, where the machine's `PATH` means nothing.
    async fn plain(req: &MachineExecRequest) -> Result<Self, ErrorResponse> {
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
        let mut env: Vec<(String, String)> = req
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if req.container.is_empty() && !env.iter().any(|(k, _)| k == "PATH") {
            env.push((
                "PATH".to_owned(),
                machine_path(&req.user, run_as.as_ref()).await,
            ));
        }
        Ok(Self {
            program: PathBuf::from(program),
            arg0: None,
            args: args.to_vec(),
            clear_env: false,
            env,
            working_dir: (!req.working_dir.is_empty()).then(|| PathBuf::from(&req.working_dir)),
            run_as,
        })
    }

    async fn login(req: &MachineExecRequest) -> Result<Self, ErrorResponse> {
        let user = if req.user.is_empty() {
            DEFAULT_LOGIN_USER
        } else {
            &req.user
        };
        let (account, run_as) = login_account(user)?;
        let path = login_path::login_path(&account, &run_as).await;
        let mut process = LoginProcess::plan(&account, &req.cmd, &path, &req.env, &req.working_dir);
        // sshd starts the session in / when the home directory is missing.
        if req.working_dir.is_empty() && !process.working_dir.is_dir() {
            process.working_dir = PathBuf::from("/");
        }
        Ok(Self {
            program: process.program,
            arg0: Some(process.arg0),
            args: process.args,
            clear_env: true,
            env: process.env,
            working_dir: Some(process.working_dir),
            run_as: Some(run_as),
        })
    }

    /// A command for this spec: program, arguments, environment and working
    /// directory.
    pub(super) fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        if let Some(arg0) = &self.arg0 {
            cmd.arg0(arg0);
        }
        cmd.args(&self.args);
        if self.clear_env {
            cmd.env_clear();
        }
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

/// The `PATH` a plain exec as `user` gets: the machine's login `PATH` for
/// the account, then whatever the agent's own adds to it. A numeric `user`
/// with no passwd entry has no profile to run, so it starts from the sshd
/// default instead.
async fn machine_path(user: &str, run_as: Option<&RunAs>) -> String {
    let user = if user.is_empty() {
        DEFAULT_LOGIN_USER
    } else {
        user
    };
    let login = match login_account(user) {
        Ok((account, login_as)) => login_path::login_path(&account, &login_as).await,
        Err(_) => login_path::sshd_default(run_as.map_or(0, |run_as| run_as.uid)).to_owned(),
    };
    match std::env::var("PATH") {
        Ok(agent) => login_path::join_paths(&login, &agent),
        Err(_) => login,
    }
}

/// The account `user` names, and the credentials a login as it gets.
fn login_account(user: &str) -> Result<(Account, RunAs), ErrorResponse> {
    let entry = lookup_account(user).map_err(|e| ErrorResponse::new(400, e))?;
    let run_as = RunAs::for_account(&entry.name, entry.uid.as_raw(), entry.gid.as_raw())
        .map_err(|e| ErrorResponse::new(500, format!("groups of {}: {e}", entry.name)))?;
    let account = Account {
        name: entry.name,
        uid: entry.uid.as_raw(),
        home: entry.dir,
        shell: entry.shell,
    };
    Ok((account, run_as))
}

/// The passwd entry of a user name or numeric uid.
fn lookup_account(user: &str) -> Result<nix::unistd::User, String> {
    let entry = match user.parse::<u32>() {
        Ok(uid) => nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)),
        Err(_) => nix::unistd::User::from_name(user),
    };
    entry
        .map_err(|e| format!("look up user {user}: {e}"))?
        .ok_or_else(|| format!("unknown user: {user}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spec(container: &str, env: &[(&str, &str)]) -> ProcessSpec {
        ProcessSpec::resolve(&MachineExecRequest {
            cmd: vec!["sh".to_owned()],
            container: container.to_owned(),
            env: env
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    fn path(spec: &ProcessSpec) -> Option<&str> {
        spec.env
            .iter()
            .find_map(|(k, v)| (k == "PATH").then_some(v.as_str()))
    }

    /// A plain exec gets the machine's login `PATH` first and keeps every
    /// directory the agent itself has, so nothing that resolved before
    /// stops resolving.
    #[tokio::test]
    async fn a_plain_exec_puts_the_login_path_in_front_of_the_agents() {
        let resolved = spec("", &[]).await;
        let path = path(&resolved).unwrap();

        let (account, run_as) = login_account(DEFAULT_LOGIN_USER).unwrap();
        let login = login_path::login_path(&account, &run_as).await;
        assert!(path.starts_with(&login), "{path} should start with {login}");

        let dirs: Vec<&str> = path.split(':').collect();
        for dir in std::env::var("PATH").unwrap().split(':') {
            assert!(dirs.contains(&dir), "{dir} missing from {path}");
        }
    }

    /// A request's own `PATH` is used as is, and a container-debug exec sets
    /// none, so it inherits the agent's: the machine's login `PATH` means
    /// nothing inside the container's namespaces.
    #[tokio::test]
    async fn a_requested_path_wins_and_a_debug_exec_inherits_the_agents() {
        let requested = spec("", &[("PATH", "/opt/bin")]).await;
        assert_eq!(path(&requested), Some("/opt/bin"));

        let debug = spec("some-container", &[]).await;
        assert_eq!(path(&debug), None);
        assert!(!debug.clear_env);
    }
}
