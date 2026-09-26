//! What a machine exec login session runs: the sshd conventions for turning
//! an account and a request into a process.
//!
//! Pure so it is testable off the guest; the passwd lookup and the spawn
//! live with the exec handler.

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

/// Directories sshd puts on `PATH` for root (Debian's build defaults).
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Directories sshd puts on `PATH` for everyone else.
const USER_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/games:/usr/games";
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
    /// SSH client joins its arguments. `env` is applied over the account's
    /// variables, and `working_dir` (when set) replaces HOME as the cwd.
    pub fn plan<S: BuildHasher>(
        account: &Account,
        cmd: &[String],
        env: &HashMap<String, String, S>,
        working_dir: &str,
    ) -> Self {
        let program = if account.shell.as_os_str().is_empty() {
            PathBuf::from(DEFAULT_SHELL)
        } else {
            account.shell.clone()
        };
        let (arg0, args) = if cmd.is_empty() {
            (login_arg0(&program), Vec::new())
        } else {
            (
                program.clone().into_os_string(),
                vec!["-c".to_owned(), cmd.join(" ")],
            )
        };

        let path = if account.uid == 0 {
            ROOT_PATH
        } else {
            USER_PATH
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

    #[test]
    fn an_empty_command_is_an_interactive_login_shell_in_home() {
        let process = LoginProcess::plan(&account(0, "/bin/bash"), &[], &HashMap::new(), "");

        assert_eq!(process.program, PathBuf::from("/bin/bash"));
        assert_eq!(process.arg0, OsString::from("-bash"));
        assert!(process.args.is_empty());
        assert_eq!(process.working_dir, PathBuf::from("/root"));
        assert_eq!(var(&process, "HOME"), Some("/root"));
        assert_eq!(var(&process, "USER"), Some("root"));
        assert_eq!(var(&process, "LOGNAME"), Some("root"));
        assert_eq!(var(&process, "SHELL"), Some("/bin/bash"));
    }

    #[test]
    fn a_command_runs_through_the_shell_joined_like_ssh_joins_arguments() {
        let cmd = ["ls".to_owned(), "-la".to_owned(), "/tmp".to_owned()];
        let process = LoginProcess::plan(&account(1000, "/bin/zsh"), &cmd, &HashMap::new(), "");

        assert_eq!(process.arg0, OsString::from("/bin/zsh"));
        assert_eq!(process.args, ["-c", "ls -la /tmp"]);
    }

    #[test]
    fn path_depends_on_whether_the_account_is_root() {
        let root = LoginProcess::plan(&account(0, "/bin/sh"), &[], &HashMap::new(), "");
        let user = LoginProcess::plan(&account(1000, "/bin/sh"), &[], &HashMap::new(), "");

        assert!(var(&root, "PATH").unwrap().contains("/usr/sbin"));
        assert!(!var(&user, "PATH").unwrap().contains("sbin"));
    }

    #[test]
    fn request_env_and_working_dir_override_the_account() {
        let env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), "/opt/bin".to_owned()),
        ]);
        let process = LoginProcess::plan(&account(0, "/bin/sh"), &[], &env, "/srv");

        assert_eq!(var(&process, "TERM"), Some("xterm-256color"));
        assert_eq!(var(&process, "PATH"), Some("/opt/bin"));
        assert_eq!(process.working_dir, PathBuf::from("/srv"));
    }

    #[test]
    fn an_account_without_a_shell_gets_bin_sh() {
        let process = LoginProcess::plan(&account(0, ""), &[], &HashMap::new(), "");

        assert_eq!(process.program, PathBuf::from("/bin/sh"));
        assert_eq!(process.arg0, OsString::from("-sh"));
        assert_eq!(var(&process, "SHELL"), Some("/bin/sh"));
    }
}
