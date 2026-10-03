//! What a machine exec login session runs: the sshd conventions for turning
//! an account and a request into a process.
//!
//! Pure so it is testable off the guest; the passwd lookup and the spawn
//! live with the exec handler.

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

/// The shell of an account whose passwd entry names none.
const DEFAULT_SHELL: &str = "/bin/sh";

/// The passwd fields a login session is built from.
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub home: PathBuf,
    /// Empty when the passwd entry names no shell.
    pub shell: PathBuf,
}

impl Account {
    /// The shell a session of this account runs: the passwd entry's, or
    /// `/bin/sh` when it names none.
    pub fn shell_program(&self) -> PathBuf {
        if self.shell.as_os_str().is_empty() {
            PathBuf::from(DEFAULT_SHELL)
        } else {
            self.shell.clone()
        }
    }
}

/// The process a login session runs, with its complete environment.
#[derive(Debug, PartialEq, Eq)]
pub struct LoginProcess {
    pub program: PathBuf,
    /// `-bash` for an interactive login shell, else the program path.
    pub arg0: OsString,
    pub args: Vec<String>,
    /// The whole environment: the caller starts the child from an empty one.
    pub env: Vec<(String, String)>,
    pub working_dir: PathBuf,
}

impl LoginProcess {
    /// Plans the login session `account` runs for a request.
    ///
    /// An empty `cmd` is an interactive login shell; otherwise `cmd` is
    /// joined with spaces into one command line for `shell -c`, the way an
    /// SSH client joins its arguments. `path` is the machine's login `PATH`
    /// for the account (the caller derives it; see `machine_exec::login_path`).
    /// `env` is applied over the account's variables, and `working_dir`
    /// (when set) replaces HOME as the cwd.
    pub fn plan<S: BuildHasher>(
        account: &Account,
        cmd: &[String],
        path: &str,
        env: &HashMap<String, String, S>,
        working_dir: &str,
    ) -> Self {
        let program = account.shell_program();
        let (arg0, args) = if cmd.is_empty() {
            (login_arg0(&program), Vec::new())
        } else {
            (
                program.clone().into_os_string(),
                vec!["-c".to_owned(), cmd.join(" ")],
            )
        };

        let home = account.home.to_string_lossy().into_owned();
        let mut vars: HashMap<String, String> = HashMap::from([
            ("HOME".to_owned(), home),
            ("USER".to_owned(), account.name.clone()),
            ("LOGNAME".to_owned(), account.name.clone()),
            ("SHELL".to_owned(), program.to_string_lossy().into_owned()),
            ("PATH".to_owned(), path.to_owned()),
        ]);
        vars.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut env: Vec<(String, String)> = vars.into_iter().collect();
        env.sort();

        let working_dir = if working_dir.is_empty() {
            account.home.clone()
        } else {
            PathBuf::from(working_dir)
        };

        Self {
            program,
            arg0,
            args,
            env,
            working_dir,
        }
    }
}

/// `-bash` for `/bin/bash`: the leading dash is how a shell learns it is a
/// login shell.
fn login_arg0(shell: &Path) -> OsString {
    let mut arg0 = OsString::from("-");
    arg0.push(shell.file_name().unwrap_or(shell.as_os_str()));
    arg0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: u32, shell: &str) -> Account {
        Account {
            name: if uid == 0 { "root" } else { "dev" }.to_owned(),
            uid,
            home: PathBuf::from(if uid == 0 { "/root" } else { "/home/dev" }),
            shell: PathBuf::from(shell),
        }
    }

    fn var<'a>(process: &'a LoginProcess, name: &str) -> Option<&'a str> {
        process
            .env
            .iter()
            .find_map(|(k, v)| (k == name).then_some(v.as_str()))
    }

    const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

    #[test]
    fn an_empty_command_is_an_interactive_login_shell_in_home() {
        let process = LoginProcess::plan(&account(0, "/bin/bash"), &[], PATH, &HashMap::new(), "");

        assert_eq!(process.program, PathBuf::from("/bin/bash"));
        assert_eq!(process.arg0, OsString::from("-bash"));
        assert_eq!(process.args, Vec::<String>::new());
        assert_eq!(process.working_dir, PathBuf::from("/root"));
        assert_eq!(var(&process, "HOME"), Some("/root"));
        assert_eq!(var(&process, "USER"), Some("root"));
        assert_eq!(var(&process, "LOGNAME"), Some("root"));
        assert_eq!(var(&process, "SHELL"), Some("/bin/bash"));
        assert_eq!(var(&process, "PATH"), Some(PATH));
    }

    #[test]
    fn a_command_runs_through_the_shell_joined_like_ssh_joins_arguments() {
        let cmd = ["ls".to_owned(), "-la".to_owned(), "/tmp".to_owned()];
        let process =
            LoginProcess::plan(&account(1000, "/bin/zsh"), &cmd, PATH, &HashMap::new(), "");

        assert_eq!(process.arg0, OsString::from("/bin/zsh"));
        assert_eq!(process.args, ["-c", "ls -la /tmp"]);
    }

    #[test]
    fn request_env_and_working_dir_override_the_account() {
        let env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), "/opt/bin".to_owned()),
        ]);
        let process = LoginProcess::plan(&account(0, "/bin/sh"), &[], PATH, &env, "/srv");

        assert_eq!(var(&process, "TERM"), Some("xterm-256color"));
        assert_eq!(var(&process, "PATH"), Some("/opt/bin"));
        assert_eq!(process.working_dir, PathBuf::from("/srv"));
    }

    #[test]
    fn an_account_without_a_shell_gets_bin_sh() {
        let process = LoginProcess::plan(&account(0, ""), &[], PATH, &HashMap::new(), "");

        assert_eq!(process.program, PathBuf::from("/bin/sh"));
        assert_eq!(process.arg0, OsString::from("-sh"));
        assert_eq!(var(&process, "SHELL"), Some("/bin/sh"));
    }
}
