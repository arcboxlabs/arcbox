//! Temporary-root tests for `abctl uninstall`.
//!
//! Every path the command touches comes from [`Roots`], so a complete fake
//! install is laid out under a temporary directory and removed for real.
//! External commands go through a recording [`Host`] that answers "not
//! running" and "not loaded" unless a test says otherwise.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::Mutex;

use anyhow::Result;
use arcbox_constants::paths::{ArcboxProfile, DOCKER_CLI_TOOLS, privileged};
use arcbox_docker::DockerContextManager;

use super::host::Host;
use super::inventory::{Roots, scan};
use super::steps::Outcome;
use super::{Step, run};
use crate::commands::setup;

type Answer = Box<dyn Fn(&str, &[&OsStr]) -> Option<Output> + Send + Sync>;

/// Records every command and answers each with `answer`, or with a silent
/// success when `answer` declines.
struct Recorder {
    calls: Mutex<Vec<String>>,
    answer: Answer,
}

impl Recorder {
    fn new() -> Self {
        Self::answering(|_, _| None)
    }

    fn answering(
        answer: impl Fn(&str, &[&OsStr]) -> Option<Output> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Host for Recorder {
    fn run(&self, program: &str, args: &[&OsStr]) -> Result<Output> {
        let line = std::iter::once(program.to_owned())
            .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.lock().unwrap().push(line.clone());
        if let Some(output) = (self.answer)(program, args) {
            return Ok(output);
        }
        // What a clean Mac answers: the app is not running, launchd has no
        // such job, nothing is in the keychain.
        Ok(match (program, line.as_str()) {
            ("osascript", _) => output(0, "false\n", ""),
            (_, line) if line.contains("launchctl bootout") => {
                output(3, "", "Boot-out failed: 3: No such process\n")
            }
            ("security", line) if line.contains("find-certificate") => {
                output(44, "", "could not be found\n")
            }
            _ => output(0, "", ""),
        })
    }
}

fn output(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

/// A complete production install laid out under one temporary directory.
struct Install {
    _dir: tempfile::TempDir,
    roots: Roots,
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn link(path: &Path, target: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(target, path).unwrap();
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let roots = Roots {
            profile: ArcboxProfile::Production,
            data_dir: home.join(".arcbox"),
            docker_config: home.join(".docker"),
            docker_context: "arcbox".to_owned(),
            system: dir.path().join("system"),
            home,
        };
        let data = &roots.data_dir;
        let library = roots.home.join("Library");

        // The privileged helper and what it manages.
        write(&roots.system_path(privileged::HELPER_BINARY), "helper");
        write(&roots.system_path(privileged::HELPER_PLIST), "<plist/>");
        write(&roots.system_path(privileged::HELPER_SOCKET), "");
        write(&roots.system_path("/var/log/arcbox/helper.log"), "{}");
        write(
            &roots.system_path("/etc/resolver/arcbox.local"),
            "# managed by arcbox-helper\nnameserver 127.0.0.1\nport 5553\n",
        );
        write(
            &roots.hosts(),
            "##\n127.0.0.1\tlocalhost\n127.0.0.1\tArcBox\t# managed by arcbox-helper\n",
        );
        link(
            &roots.system_path(privileged::DOCKER_SOCKET),
            "/Users/alice/.arcbox/run/docker.sock",
        );
        let bin = roots.system_path("/usr/local/bin");
        for name in DOCKER_CLI_TOOLS {
            link(
                &bin.join(name),
                &format!("/Applications/ArcBox.app/Contents/MacOS/xbin/{name}"),
            );
        }
        link(
            &bin.join("abctl"),
            "/Applications/ArcBox.app/Contents/MacOS/bin/abctl",
        );

        // The user's own.
        write(
            &roots.launch_agent("com.arcboxlabs.desktop.daemon"),
            "<plist/>",
        );
        write(&roots.launch_agent("dev.arcbox.daemon"), "<plist/>");
        for sub in [
            "Application Support/com.arcboxlabs.desktop/state.json",
            "Caches/com.arcboxlabs.desktop/cache",
            "HTTPStorages/com.arcboxlabs.desktop/cookies",
            "Saved Application State/com.arcboxlabs.desktop.savedState/window",
            "Logs/arcbox/daemon.stdout.log",
        ] {
            write(&library.join(sub), "");
        }
        write(&roots.preferences(), "<plist/>");
        write(&data.join("data/docker.img"), "disk");
        write(&data.join("run/daemon.lock"), "");
        write(&data.join("bin/abctl"), "");
        write(&data.join("shell/init.zsh"), "");
        write(&data.join("tls/ca.pem"), "pem");
        write(&roots.app_bundle().join("Contents/Info.plist"), "<plist/>");

        // Integrations.
        let manager = DockerContextManager::with_config_dir(
            data.join("run/docker.sock"),
            roots.docker_config.clone(),
        );
        write(
            &roots.docker_config.join("config.json"),
            r#"{"currentContext":"desktop-linux","credsStore":"osxkeychain"}"#,
        );
        manager.enable().unwrap();
        write(
            &roots.home.join(".ssh/config"),
            "# Added by `abctl ssh install`: ssh <machine>@arcbox\nInclude ~/.arcbox/ssh/config\n\nHost *\n  User me\n",
        );

        Self { _dir: dir, roots }
    }

    /// Writes the shell profile under THIS home, as `setup install` leaves
    /// it, and returns its path. The shell is the one `setup` detects from
    /// `$SHELL` (zsh here, bash on a CI runner), and the path must come from
    /// the install's home, never from the process's own: a probe of the real
    /// login shell once pointed the removal at the developer's profile.
    async fn write_shell_profile(&self) -> PathBuf {
        let integration = setup::Integration::under(
            &self.roots.home,
            &self.roots.data_dir,
            self.roots.docker_config.clone(),
        )
        .await
        .unwrap();
        let init = self
            .roots
            .data_dir
            .join(format!("shell/init.{}", integration.shell_kind.as_str()));
        let source = match integration.shell_kind {
            setup::ShellKind::Fish => format!("source \"{}\"; or true", init.display()),
            setup::ShellKind::Zsh | setup::ShellKind::Bash => {
                format!("source \"{}\" 2>/dev/null || :", init.display())
            }
        };
        write(
            &integration.profile,
            &format!(
                "export KEEP=1\n\n# Added by ArcBox: command-line tools and integration\n{source} # managed by ArcBox\n"
            ),
        );
        integration.profile
    }

    fn roots(&self) -> &Roots {
        &self.roots
    }

    fn docker(&self) -> DockerContextManager {
        DockerContextManager::with_config_dir(
            self.roots.data_dir.join("run/docker.sock"),
            self.roots.docker_config.clone(),
        )
    }
}

async fn uninstall(install: &Install, host: &dyn Host, keep_data: bool) -> Vec<Step> {
    let residue = scan(install.roots(), keep_data);
    run(host, install.roots(), &residue, &mut |_| {}).await
}

fn failures(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .filter(|step| step.failed())
        .map(ToString::to_string)
        .collect()
}

#[tokio::test]
async fn everything_arcbox_wrote_is_removed_and_nothing_else_is_touched() {
    let install = Install::new();
    let roots = install.roots();
    // OrbStack's link shares our xbin layout (#715); a stray real file in
    // /usr/local/bin is nobody's to delete.
    let bin = roots.system_path("/usr/local/bin");
    fs::remove_file(bin.join("docker-compose")).unwrap();
    link(
        &bin.join("docker-compose"),
        "/Applications/OrbStack.app/Contents/MacOS/xbin/docker-compose",
    );
    write(&bin.join("docker-credential-pass"), "real binary");
    write(&roots.home.join("ArcBox/README"), "the user's own folder");
    // A mount point the daemon left behind under the machine mount root.
    fs::create_dir_all(roots.machine_mount_root().join("ubuntu")).unwrap();
    let shell_profile = install.write_shell_profile().await;

    let host = Recorder::new();
    let steps = uninstall(&install, &host, false).await;
    assert_eq!(failures(&steps), Vec::<String>::new());

    for absent in [
        roots.system_path(privileged::HELPER_BINARY),
        roots.system_path(privileged::HELPER_PLIST),
        roots.system_path(privileged::HELPER_SOCKET),
        roots.system_path("/var/log/arcbox"),
        roots.system_path("/etc/resolver/arcbox.local"),
        roots.system_path(privileged::DOCKER_SOCKET),
        bin.join("docker"),
        bin.join("docker-buildx"),
        bin.join("docker-credential-osxkeychain"),
        bin.join("abctl"),
        roots.launch_agent("com.arcboxlabs.desktop.daemon"),
        roots.launch_agent("dev.arcbox.daemon"),
        roots
            .home
            .join("Library/Application Support/com.arcboxlabs.desktop"),
        roots.home.join("Library/Caches/com.arcboxlabs.desktop"),
        roots
            .home
            .join("Library/HTTPStorages/com.arcboxlabs.desktop"),
        roots
            .home
            .join("Library/Saved Application State/com.arcboxlabs.desktop.savedState"),
        roots.home.join("Library/Logs/arcbox"),
        roots.preferences(),
        roots.data_dir.clone(),
        roots.app_bundle(),
        roots.machine_mount_root(),
    ] {
        assert!(
            absent.symlink_metadata().is_err(),
            "{} should be gone",
            absent.display()
        );
    }
    assert_eq!(
        fs::read_link(bin.join("docker-compose")).unwrap(),
        PathBuf::from("/Applications/OrbStack.app/Contents/MacOS/xbin/docker-compose")
    );
    assert_eq!(
        fs::read_to_string(bin.join("docker-credential-pass")).unwrap(),
        "real binary"
    );
    assert_eq!(
        fs::read_to_string(roots.hosts()).unwrap(),
        "##\n127.0.0.1\tlocalhost\n"
    );
    assert!(roots.home.join("ArcBox/README").exists());

    // The context in use is removed and the previous one restored (#716).
    let docker = install.docker();
    assert!(!docker.context_exists());
    assert_eq!(
        docker.current_context().unwrap().as_deref(),
        Some("desktop-linux")
    );
    assert_eq!(
        fs::read_to_string(roots.home.join(".ssh/config")).unwrap(),
        "Host *\n  User me\n"
    );
    assert_eq!(
        fs::read_to_string(&shell_profile).unwrap(),
        "export KEEP=1\n"
    );

    // launchd is asked by label, never `pkill`; the helper is unregistered
    // with sudo, and preferences go through `defaults`.
    let calls = host.calls();
    let uid = unsafe { libc::getuid() };
    assert!(calls.contains(&format!(
        "launchctl bootout gui/{uid}/com.arcboxlabs.desktop.daemon"
    )));
    assert!(
        calls.contains(&"sudo launchctl bootout system/com.arcboxlabs.desktop.helper".to_owned())
    );
    assert!(calls.contains(&"defaults delete com.arcboxlabs.desktop".to_owned()));
    assert!(
        !calls.iter().any(|call| call.contains("pkill")),
        "{calls:?}"
    );
}

/// A directory under the machine mount root that holds the user's own files
/// is not a mount point the daemon left: it and the root stay.
#[tokio::test]
async fn a_machine_mount_root_with_the_users_files_is_left_alone() {
    let install = Install::new();
    let roots = install.roots();
    write(
        &roots.machine_mount_root().join("ubuntu/notes.txt"),
        "kept after the mount went away",
    );

    let steps = uninstall(&install, &Recorder::new(), false).await;
    assert_eq!(failures(&steps), Vec::<String>::new());
    let step = steps
        .iter()
        .find(|step| step.label.starts_with("Unmounting machines"))
        .expect("the machines step ran");
    assert!(
        matches!(&step.outcome, Ok(Outcome::Skipped(reason)) if reason.contains("left alone")),
        "{step}"
    );
    assert!(roots.machine_mount_root().join("ubuntu/notes.txt").exists());
}

#[tokio::test]
async fn keep_data_leaves_the_data_directory_and_homebrew_keeps_its_app() {
    let install = Install::new();
    let roots = install.roots();
    fs::create_dir_all(roots.system_path("/opt/homebrew/Caskroom/arcbox/1.37.0")).unwrap();

    let residue = scan(roots, true);
    assert!(residue.app_left_to_homebrew);
    let steps = run(&Recorder::new(), roots, &residue, &mut |_| {}).await;
    assert_eq!(failures(&steps), Vec::<String>::new());

    assert_eq!(
        fs::read_to_string(roots.data_dir.join("data/docker.img")).unwrap(),
        "disk"
    );
    assert!(!roots.data_dir.join("bin").exists());
    assert!(!roots.data_dir.join("run").exists());
    assert!(roots.app_bundle().exists());
}

/// A step that fails is reported as failed, and the rest still run (#716).
#[tokio::test]
async fn a_failed_step_is_reported_and_does_not_stop_the_others() {
    let install = Install::new();
    let host = Recorder::answering(|program, args| {
        let line = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        (program == "sudo" && line.contains("launchctl bootout system/"))
            .then(|| output(1, "", "Boot-out failed: 5: Input/output error\n"))
    });

    let steps = uninstall(&install, &host, false).await;
    let failed = failures(&steps);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0].starts_with("Unregistering the helper"),
        "{failed:?}"
    );
    assert!(failed[0].contains("Input/output error"), "{failed:?}");
    assert!(!install.roots().data_dir.exists());
}

/// A daemon started by `abctl daemon start` has no launchd job: it is found
/// through `daemon.lock` and stopped with SIGTERM (#716).
#[tokio::test]
async fn a_daemon_without_a_launchd_job_is_stopped_through_its_lock() {
    let install = Install::new();
    let lock = install.roots().data_dir.join("run/daemon.lock");
    // Stands in for the daemon: takes the flock and writes its PID, as the
    // daemon does, then waits for SIGTERM.
    let mut child = std::process::Command::new("/usr/bin/perl")
        .args([
            "-MFcntl=:flock",
            "-e",
            r#"open(my $f, ">", $ARGV[0]) or die; flock($f, LOCK_EX) or die; syswrite($f, "$$\n"); sleep 60 while 1;"#,
            lock.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !super::super::daemon::daemon_is_alive(&lock) {
        assert!(
            std::time::Instant::now() < deadline,
            "stand-in never took the lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let steps = uninstall(&install, &Recorder::new(), false).await;
    assert_eq!(failures(&steps), Vec::<String>::new());
    let stop = steps
        .iter()
        .find(|step| step.label == "Stopping the daemon")
        .unwrap();
    assert!(matches!(stop.outcome, Ok(Outcome::Done)), "{stop}");
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}
