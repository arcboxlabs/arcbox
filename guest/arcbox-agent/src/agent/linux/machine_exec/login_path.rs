//! The machine's login `PATH`: what a login shell ends up with once the
//! machine's profile has had its say.
//!
//! sshd hands every session its compiled-in default `PATH`, and the
//! distro's `/etc/profile` (then the account's own profile) rewrites it —
//! Debian keeps the default, NixOS replaces it with
//! `/run/current-system/sw/bin` and friends. A process that inherits only
//! the agent's `PATH`, or only the sshd default, finds nothing on NixOS. The
//! probe runs that login sequence for real: `/bin/sh -l` as the account,
//! starting from the sshd default, reporting the resulting `PATH` over a
//! descriptor of its own so that profile chatter cannot corrupt it.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use arcbox_pty::RunAs;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::agent::login_session::Account;

/// Directories sshd puts on `PATH` for root (Debian's build defaults).
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Directories sshd puts on `PATH` for everyone else.
const USER_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/games:/usr/games";
/// Every distro keeps its profile POSIX-compatible for this shell.
const PROBE_SHELL: &str = "/bin/sh";
/// A profile that blocks (prompting, waiting on the network) must not hold
/// the exec; past this the sshd default stands.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// The descriptor the probe reports `PATH` on; stdout and stderr belong to
/// whatever the profile prints.
const RESULT_FD: i32 = 3;

/// The `PATH` sshd starts a session of `uid` from.
pub(super) fn sshd_default(uid: u32) -> &'static str {
    if uid == 0 { ROOT_PATH } else { USER_PATH }
}

/// The `PATH` a login of `account` ends up with — the sshd default when the
/// probe fails, so an exec is never refused for it.
pub(super) async fn login_path(account: &Account, run_as: &RunAs) -> String {
    let default = sshd_default(account.uid);
    match probe(account, run_as, default).await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                user = %account.name,
                %error,
                "login PATH probe failed; using the sshd default"
            );
            default.to_owned()
        }
    }
}

async fn probe(account: &Account, run_as: &RunAs, default: &str) -> io::Result<String> {
    let (reader, writer) = io::pipe()?;
    let working_dir = if account.home.is_dir() {
        account.home.as_path()
    } else {
        Path::new("/")
    };
    let mut command = Command::new(PROBE_SHELL);
    command
        .arg("-l")
        .arg("-c")
        .arg(format!("printf '%s' \"$PATH\" >&{RESULT_FD}"))
        .env_clear()
        .env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("SHELL", account.shell_program())
        .env("PATH", default)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let run_as = run_as.clone();
    let writer_fd = writer.as_raw_fd();
    // SAFETY: the closure runs in the forked child and only calls `dup2`,
    // `fcntl` and the credential syscalls `RunAs::apply` documents as
    // async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            // `dup2` clears close-on-exec on the result, which is what lets
            // the shell see fd 3; when the pipe already sits there, clear it
            // by hand.
            if writer_fd == RESULT_FD {
                if libc::fcntl(RESULT_FD, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(writer_fd, RESULT_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            run_as.apply()
        });
    }
    let mut child = command.spawn()?;
    // The child holds the only other copy: EOF means it is done with fd 3.
    drop(writer);

    let mut reader = tokio::net::unix::pipe::Receiver::from_owned_fd(OwnedFd::from(reader))?;
    let mut path = Vec::new();
    let status = tokio::time::timeout(PROBE_TIMEOUT, async {
        reader.read_to_end(&mut path).await?;
        child.wait().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the profile did not finish"))??;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{PROBE_SHELL} -l exited with {status}"
        )));
    }
    let path = String::from_utf8(path)
        .map_err(|_| io::Error::other("the profile left PATH as non-UTF-8"))?;
    if path.is_empty() || path.contains(['\n', '\0']) {
        return Err(io::Error::other(format!(
            "the profile left PATH unusable: {path:?}"
        )));
    }
    Ok(path)
}

/// `first` followed by the directories of `second` it does not already
/// have, in order.
pub(super) fn join_paths(first: &str, second: &str) -> String {
    let mut dirs: Vec<&str> = first.split(':').filter(|dir| !dir.is_empty()).collect();
    for dir in second.split(':').filter(|dir| !dir.is_empty()) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sshd_default_gives_sbin_to_root_only() {
        assert!(sshd_default(0).contains("/usr/sbin"));
        assert!(!sshd_default(1000).contains("sbin"));
    }

    #[test]
    fn joined_paths_keep_the_first_set_in_front_and_add_only_new_directories() {
        assert_eq!(
            join_paths("/run/current-system/sw/bin:/bin", "/bin:/sbin:/usr/bin"),
            "/run/current-system/sw/bin:/bin:/sbin:/usr/bin"
        );
        assert_eq!(join_paths("/opt/bin", ""), "/opt/bin");
        assert_eq!(join_paths("", "/bin::/sbin"), "/bin:/sbin");
    }
}
