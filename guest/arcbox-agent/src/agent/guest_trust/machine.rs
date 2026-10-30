//! Rebuild supported distro stores from an authenticated ArcBox source anchor.

use std::{
    fs,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use arcbox_local_ca::{PublicAuthority, install_trust_anchor, reconcile_trust_bundle};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use wait_timeout::ChildExt as _;

const BUNDLE: &str = "etc/ssl/certs/ca-certificates.crt";
const ANCHOR: &str = "usr/local/share/ca-certificates/arcbox-local-ca.crt";
const UPDATER: &str = "usr/sbin/update-ca-certificates";

pub(super) fn install(root: &Path, authority: Option<&PublicAuthority>) -> Result<()> {
    let release =
        fs::read_to_string(root.join("etc/os-release")).context("read machine distribution")?;
    let id = release
        .lines()
        .find_map(|line| line.strip_prefix("ID="))
        .map(|value| value.trim_matches(['\'', '"']));
    ensure!(
        matches!(id, Some("debian" | "ubuntu" | "alpine")),
        "unsupported machine trust distribution: {id:?}"
    );
    let bundle = root.join(BUNDLE);
    let anchor = root.join(ANCHOR);
    let updater = root.join(UPDATER);
    ensure!(
        fs::symlink_metadata(&bundle)
            .context("machine trust bundle is unavailable")?
            .is_file(),
        "unsupported machine trust bundle: {}",
        bundle.display()
    );
    let executable = fs::metadata(&updater).context("machine trust updater is unavailable")?;
    ensure!(
        executable.is_file() && executable.permissions().mode() & 0o111 != 0,
        "machine trust updater is not executable"
    );
    install_trust_anchor(&anchor, authority)?;
    update(&updater, Duration::from_secs(30))?;
    let installed = fs::read(&bundle).context("read rebuilt machine trust bundle")?;
    ensure!(
        reconcile_trust_bundle(&installed, authority)? == installed,
        "machine trust updater did not apply the requested root"
    );
    fs::File::open(&bundle)?.sync_all()?;
    fs::File::open(bundle.parent().expect("fixed bundle path has a parent"))?.sync_all()?;
    Ok(())
}

fn update(program: &Path, timeout: Duration) -> Result<()> {
    let mut child = Command::new(program)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .context("start machine trust updater")?;
    let waited = child.wait_timeout(timeout);
    if let Ok(Some(status)) = &waited {
        ensure!(status.success(), "machine trust updater failed: {status}");
        return Ok(());
    }
    // Hooks share this process group and must stop before the Store releases its writer.
    let killed =
        killpg(Pid::from_raw(child.id().cast_signed()), Signal::SIGKILL).or_else(|error| {
            if error == Errno::ESRCH {
                Ok(())
            } else {
                Err(error)
            }
        });
    let reaped = child.wait();
    killed.context("stop machine trust updater and hooks")?;
    reaped.context("reap machine trust updater")?;
    waited.context("wait for machine trust updater")?;
    bail!("machine trust updater timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for path in [BUNDLE, ANCHOR, UPDATER] {
            fs::create_dir_all(dir.path().join(path).parent().unwrap()).unwrap();
        }
        fs::write(dir.path().join("etc/os-release"), "ID=debian\n").unwrap();
        fs::write(dir.path().join(BUNDLE), "# original roots\n").unwrap();
        dir
    }

    fn updater(root: &Path, body: &str) {
        let path = root.join(UPDATER);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn replay_repairs_an_anchor_published_before_an_updater_failure() {
        let dir = setup();
        let root = dir.path();
        let ca =
            PublicAuthority::from_pem(include_bytes!("../linux/rpc/guest_trust_ca.pem")).unwrap();
        updater(root, "exit 9");
        assert!(
            install(root, Some(&ca))
                .unwrap_err()
                .to_string()
                .contains("failed")
        );
        assert_eq!(fs::read(root.join(BUNDLE)).unwrap(), b"# original roots\n");
        assert!(root.join(ANCHOR).is_file());
        updater(
            root,
            &format!(
                "{{ printf '# original roots\\n'; if test -f '{0}'; then cat '{0}'; fi; }} > '{1}'",
                root.join(ANCHOR).display(),
                root.join(BUNDLE).display()
            ),
        );
        install(root, Some(&ca)).unwrap();
        install(root, Some(&ca)).unwrap();
        install(root, None).unwrap();
        install(root, None).unwrap();
        assert_eq!(fs::read(root.join(BUNDLE)).unwrap(), b"# original roots\n");
        assert!(!root.join(ANCHOR).exists());
    }

    #[test]
    fn missing_prerequisites_and_false_success_do_not_acknowledge_installation() {
        let dir = setup();
        let root = dir.path();
        let ca =
            PublicAuthority::from_pem(include_bytes!("../linux/rpc/guest_trust_ca.pem")).unwrap();
        assert!(install(root, Some(&ca)).is_err());
        assert!(!root.join(ANCHOR).exists());
        updater(root, "exit 0");
        fs::write(root.join("etc/os-release"), "ID=nixos\n").unwrap();
        assert!(install(root, Some(&ca)).is_err());
        assert!(!root.join(ANCHOR).exists());
        fs::write(root.join("etc/os-release"), "ID=debian\n").unwrap();
        assert!(
            install(root, Some(&ca))
                .unwrap_err()
                .to_string()
                .contains("did not apply")
        );
        fs::remove_file(root.join(BUNDLE)).unwrap();
        let anchor = fs::read(root.join(ANCHOR)).unwrap();
        assert!(
            install(root, None)
                .unwrap_err()
                .to_string()
                .contains("bundle is unavailable")
        );
        assert_eq!(fs::read(root.join(ANCHOR)).unwrap(), anchor);
    }

    #[test]
    fn timeout_stops_hooks_and_reaps_the_updater_before_returning() {
        let dir = setup();
        let pid = dir.path().join("updater.pid");
        let hook = dir.path().join("hook.pid");
        updater(
            dir.path(),
            &format!(
                "echo $$ > '{}'; sleep 60 & echo $! > '{}'; wait",
                pid.display(),
                hook.display()
            ),
        );
        let error = update(&dir.path().join(UPDATER), Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let parent: i32 = fs::read_to_string(pid).unwrap().trim().parse().unwrap();
        assert_eq!(
            nix::sys::signal::kill(Pid::from_raw(parent), None),
            Err(Errno::ESRCH)
        );
        let hook: u32 = fs::read_to_string(hook).unwrap().trim().parse().unwrap();
        let stat = Path::new("/proc").join(hook.to_string()).join("stat");
        let stopped = || match fs::read_to_string(&stat) {
            Ok(value) => value.rsplit_once(") ").unwrap().1.starts_with('Z'),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        for _ in 0..100 {
            if stopped() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the updater hook outlived its owner");
    }
}
