//! Real Docker stand-ins verify cancellation, child reaping, and command admission.

use std::ffi::OsString;
use std::time::Duration;

use super::*;

struct RestorePath(Option<OsString>);

impl Drop for RestorePath {
    fn drop(&mut self) {
        restore_path(self.0.take());
    }
}

struct KillDescendant(PathBuf);

impl Drop for KillDescendant {
    fn drop(&mut self) {
        let pid = match std::fs::read_to_string(&self.0) {
            Ok(pid) => pid.trim().parse::<libc::pid_t>().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("failed to read fixture descendant PID: {error}"),
        };
        // SAFETY: the fixture owns this descendant, which sleeps until cleanup.
        let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(
            killed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "failed to kill fixture descendant"
        );
    }
}

const BLOCKING_DOCKER: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.calls"
case "$*" in
  *"BLOCK_COMMAND"*)
    BLOCK_PROCESS
    ;;
  *"info --format"*)
    echo '{"Name":"orbstack","ServerVersion":"29.0","OperatingSystem":"OrbStack","Architecture":"aarch64"}'
    ;;
esac
exit 0
"#;

fn assert_cancelled(error: &CoreError) {
    assert!(
        matches!(
            error,
            CoreError::Common(arcbox_error::CommonError::Io(error))
                if error.kind() == std::io::ErrorKind::Interrupted
        ),
        "expected cancellation, got {error}"
    );
}

#[tokio::test]
async fn prepare_cancellation_reaps_source_and_target_commands_and_rejects_later_commands() {
    let _children = CHILD_PROCESSES.write().await;
    for (blocked_command, expected_calls, inherited_pipes) in [
        ("info --format", 1, false),
        ("image ls -aq", 2, false),
        ("info --format", 1, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let descendant = KillDescendant(dir.path().join("docker.descendant.pid"));
        let blocker = if inherited_pipes {
            r#"/bin/sleep 600 &
    printf '%s\n' "$!" > "$0.descendant.pid"
    printf '%s\n' "$$" > "$0.pid"
    wait"#
        } else {
            r#"printf '%s\n' "$$" > "$0.pid"
    exec /bin/sleep 600"#
        };
        let _path = RestorePath(install_docker_shim(
            dir.path(),
            &BLOCKING_DOCKER
                .replace("BLOCK_COMMAND", blocked_command)
                .replace("BLOCK_PROCESS", blocker),
        ));
        let socket = dir.path().join("docker.sock");
        std::fs::write(&socket, "").unwrap();
        let manager = MigrationManager::new(dir.path().join("arcbox-docker.sock"));
        let cancelled = CancellationToken::new();
        let mut preparing =
            Box::pin(manager.prepare_migration(prepare_request(&socket, false), cancelled.clone()));
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            let mut poll = tokio::time::interval(Duration::from_millis(10));
            loop {
                tokio::select! {
                    result = &mut preparing => panic!("prepare exited before cancellation: {result:?}"),
                    _ = poll.tick() => {
                        if let Ok(pid) = std::fs::read_to_string(dir.path().join("docker.pid"))
                            && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
                        {
                            break pid;
                        }
                    }
                }
            }
        })
        .await
        .expect("Docker stand-in did not start");

        cancelled.cancel();
        let error = tokio::time::timeout(Duration::from_secs(3), &mut preparing)
            .await
            .expect("prepare did not join the cancelled command")
            .unwrap_err();
        assert_cancelled(&error);
        let mut status = 0;
        // SAFETY: the fixture recorded this child PID, and status is a valid output pointer.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        let wait_error = std::io::Error::last_os_error();
        assert_eq!(waited, -1, "prepare returned before reaping the child");
        assert_eq!(wait_error.raw_os_error(), Some(libc::ECHILD));
        if inherited_pipes {
            let pid = std::fs::read_to_string(&descendant.0)
                .unwrap()
                .trim()
                .parse::<libc::pid_t>()
                .unwrap();
            // SAFETY: signal zero only checks the descendant recorded by this fixture.
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        }

        let calls = std::fs::read_to_string(dir.path().join("docker.calls")).unwrap();
        assert_eq!(calls.lines().count(), expected_calls);
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            manager.prepare_migration(prepare_request(&socket, false), cancelled),
        )
        .await
        .expect("cancelled preparation started another command")
        .unwrap_err();
        assert_cancelled(&error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("docker.calls")).unwrap(),
            calls
        );
        assert!(manager.prepared.read().await.is_empty());
    }
}

#[tokio::test]
async fn prepare_preserves_a_failed_docker_commands_stderr() {
    let _children = CHILD_PROCESSES.write().await;
    let dir = tempfile::tempdir().unwrap();
    let _path = RestorePath(install_docker_shim(
        dir.path(),
        "#!/bin/sh\nprintf 'fixture docker failure\\n' >&2\nexit 7\n",
    ));
    let socket = dir.path().join("docker.sock");
    std::fs::write(&socket, "").unwrap();
    let manager = MigrationManager::new(dir.path().join("arcbox-docker.sock"));
    let error = manager
        .prepare_migration(prepare_request(&socket, false), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture docker failure"));
}
